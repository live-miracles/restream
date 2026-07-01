#!/usr/bin/env bash
set -euo pipefail

SCRIPT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEFAULT_LOCK_FILE="/tmp/restream-build.lock"
DEFAULT_LOCAL_ARTIFACT_KEEP="${RESTREAM_AGENT_WORKTREE_LOCAL_ARTIFACT_KEEP:-3}"
DEFAULT_INCREMENTAL_KEEP="${RESTREAM_AGENT_WORKTREE_INCREMENTAL_KEEP:-2}"

WORKTREE_ID=""
WORKTREE_PATH=""
BRANCH_NAME=""
BASE_REF="HEAD"
SOURCE_TREE="$SCRIPT_ROOT"
DRY_RUN=0
DO_CLEANUP=0
FORCE_CLEANUP=0
SEED_TARGET=1
SEED_CARGO=1
SEED_NODE=1
SHARE_STATIC=1
TARGET_CACHE_MODE="subset"
WITH_INCREMENTAL=0

LOCAL_DEP_PREFIXES=(
    librestream
    restream
    restream_mcp
    test_harness
)

ROOT_DEBUG_FILES=(
    librestream.d
    librestream.rlib
    restream
    restream.d
    restream-mcp
    restream-mcp.d
    test_harness
    test_harness.d
)

INCREMENTAL_PREFIXES=(
    build_script_build
    restream
    restream_mcp
    test_harness
)

usage() {
    cat <<'EOF'
Usage: scripts/agent-worktree.sh [options] <id>

Create, refresh, or clean up an agent worktree with warm caches and shared
static outputs.

Defaults:
  - new worktree path: <repo-common-root>/worktrees/<id>
  - branch name: codex/<id>
  - source cache tree: current worktree
  - seed a pruned high-value debug target subset from the source tree
  - do not copy incremental compilation state unless explicitly requested
  - seed .cargo/ and node_modules/ from the source tree when present
  - share .build/static and public/bin from the source tree when present

Options:
  --cleanup                remove the worktree for <id> instead of creating/updating it
  --force-cleanup          force cleanup even if the worktree is dirty or locked
  --path <path>            override the destination worktree path
  --branch <name>          override the branch name (default: codex/<id>)
  --base <rev>             base revision for a new branch (default: HEAD)
  --source <path>          source worktree used for cache/static seeding
  --full-target-cache      copy the full target/ tree instead of the pruned debug subset
  --with-incremental       include a small pruned debug incremental cache slice
  --no-target-cache        skip target/ seeding
  --no-cargo-config        skip .cargo/ seeding
  --no-node-modules        skip node_modules/ seeding
  --no-share-static        do not share .build/static or public/bin
  --dry-run                print actions without mutating the repo
  -h, --help               show this help

Notes:
  - Cleanup removes the worktree checkout. It does not delete the git branch.
  - Static sharing is meant for tasks that do not modify the native/static
    build layer. Disable it with --no-share-static when working on that layer.
  - The default target warmup copies only high-value debug artifacts:
    debug/deps, debug/build, debug/.fingerprint, root debug binaries, and no
    incremental state unless --with-incremental is set.
  - Per-worktree target/ state is copied, not shared, so concurrent worktrees
    do not write into the same cache or incremental directory.
EOF
}

info() {
    echo "agent-worktree: $*"
}

warn() {
    echo "agent-worktree: $*" >&2
}

die() {
    warn "$*"
    exit 1
}

run_cmd() {
    if ((DRY_RUN)); then
        printf 'dry-run:'
        for arg in "$@"; do
            printf ' %q' "$arg"
        done
        printf '\n'
        return 0
    fi
    "$@"
}

resolve_existing_path() {
    local path="$1"
    if [[ "$path" = /* ]]; then
        (cd "$path" && pwd -P)
    else
        (cd "$SCRIPT_ROOT/$path" && pwd -P)
    fi
}

resolve_target_path() {
    local path="$1"
    if [[ "$path" = /* ]]; then
        printf '%s\n' "$path"
    else
        printf '%s/%s\n' "$PWD" "$path"
    fi
}

resolve_common_repo_root() {
    local source_root="$1"
    local common_dir
    common_dir="$(git -C "$source_root" rev-parse --git-common-dir)"
    if [[ "$common_dir" != /* ]]; then
        common_dir="$(cd "$source_root/$common_dir" && pwd -P)"
    fi
    dirname "$common_dir"
}

cleanup_worktree() {
    local source_tree="$1"
    local worktree_path="$2"
    local force_cleanup="$3"
    local args=(worktree remove)

    if [[ ! -d "$worktree_path" && ! -L "$worktree_path" ]]; then
        die "cleanup target does not exist: $worktree_path"
    fi

    if ((force_cleanup)); then
        args+=(-f)
    fi
    args+=("$worktree_path")

    info "cleanup worktree: $worktree_path"
    run_cmd git -C "$source_tree" "${args[@]}"
    info "cleanup stale worktree metadata"
    run_cmd git -C "$source_tree" worktree prune
}

sync_tree() {
    local source_path="$1"
    local target_path="$2"
    local label="$3"
    local rsync_status=0

    if [[ ! -e "$source_path" ]]; then
        info "skip ${label}: source missing at $source_path"
        return 0
    fi

    if [[ "$source_path" -ef "$target_path" ]]; then
        info "skip ${label}: source and target are the same path"
        return 0
    fi

    info "seed ${label}: $source_path -> $target_path"
    if command -v rsync >/dev/null 2>&1; then
        run_cmd mkdir -p "$target_path"
        if ((DRY_RUN)); then
            run_cmd rsync -a --delete "$source_path/" "$target_path/"
            return 0
        fi

        set +e
        rsync -a --delete "$source_path/" "$target_path/"
        rsync_status=$?
        set -e

        case "$rsync_status" in
            0)
                ;;
            24)
                warn "rsync reported vanished source files while seeding ${label}; keeping the copied cache snapshot"
                ;;
            *)
                die "rsync failed while seeding ${label} (exit ${rsync_status})"
                ;;
        esac
    else
        warn "rsync not found; falling back to cp -a for ${label}"
        run_cmd rm -rf "$target_path"
        run_cmd mkdir -p "$(dirname "$target_path")"
        run_cmd cp -a "$source_path" "$target_path"
    fi
}

copy_file_if_present() {
    local source_path="$1"
    local target_path="$2"
    local label="$3"

    if [[ ! -e "$source_path" ]]; then
        info "skip ${label}: source missing at $source_path"
        return 0
    fi

    info "seed ${label}: $source_path -> $target_path"
    run_cmd mkdir -p "$(dirname "$target_path")"
    run_cmd cp -a "$source_path" "$target_path"
}

latest_hash_ids_for_prefix() {
    local source_dir="$1"
    local prefix="$2"
    local keep="$3"

    find "$source_dir" -maxdepth 1 -type f -name "${prefix}-*" -printf '%T@ %f\n' |
        awk -v prefix="${prefix}-" '
            {
                name = $2
                sub("^" prefix, "", name)
                split(name, parts, /\./)
                hash = parts[1]
                if ($1 > newest[hash]) {
                    newest[hash] = $1
                }
            }
            END {
                for (hash in newest) {
                    print newest[hash], hash
                }
            }
        ' |
        sort -nr |
        awk -v keep="$keep" 'NR <= keep { print $2 }'
}

copy_local_dep_groups() {
    local source_dir="$1"
    local target_dir="$2"
    local prefix="$3"
    local keep="$4"
    local ids=()
    local id

    if [[ ! -d "$source_dir" ]]; then
        return 0
    fi

    mapfile -t ids < <(latest_hash_ids_for_prefix "$source_dir" "$prefix" "$keep")
    if ((${#ids[@]} == 0)); then
        info "skip debug/deps ${prefix}: no matching local artifact groups"
        return 0
    fi

    info "seed debug/deps ${prefix}: keeping latest ${#ids[@]} artifact group(s)"
    run_cmd mkdir -p "$target_dir"
    for id in "${ids[@]}"; do
        if ((DRY_RUN)); then
            printf 'dry-run: copy local dep group %s-%s from %s to %s\n' "$prefix" "$id" "$source_dir" "$target_dir"
            continue
        fi

        while IFS= read -r -d '' file; do
            cp -a "$file" "$target_dir/"
        done < <(find "$source_dir" -maxdepth 1 -type f -name "${prefix}-${id}*" -print0)
    done
}

seed_debug_deps_subset() {
    local source_dir="$1"
    local target_dir="$2"
    local prefix

    if [[ ! -d "$source_dir" ]]; then
        info "skip debug/deps subset: source missing at $source_dir"
        return 0
    fi

    info "seed debug/deps subset: copy third-party artifacts and latest local crate groups"
    run_cmd mkdir -p "$target_dir"
    if command -v rsync >/dev/null 2>&1; then
        if ((DRY_RUN)); then
            run_cmd rsync -a --delete \
                --exclude 'librestream-*' \
                --exclude 'restream-*' \
                --exclude 'restream_mcp-*' \
                --exclude 'test_harness-*' \
                "$source_dir/" "$target_dir/"
        else
            local rsync_status=0
            set +e
            rsync -a --delete \
                --exclude 'librestream-*' \
                --exclude 'restream-*' \
                --exclude 'restream_mcp-*' \
                --exclude 'test_harness-*' \
                "$source_dir/" "$target_dir/"
            rsync_status=$?
            set -e
            case "$rsync_status" in
                0)
                    ;;
                24)
                    warn "rsync reported vanished source files while seeding debug/deps subset; keeping the copied cache snapshot"
                    ;;
                *)
                    die "rsync failed while seeding debug/deps subset (exit ${rsync_status})"
                    ;;
            esac
        fi
    else
        warn "rsync not found; falling back to cp -a for debug/deps subset"
        run_cmd cp -a "$source_dir/." "$target_dir/"
        for prefix in "${LOCAL_DEP_PREFIXES[@]}"; do
            run_cmd find "$target_dir" -maxdepth 1 -type f -name "${prefix}-*" -delete
        done
    fi

    for prefix in "${LOCAL_DEP_PREFIXES[@]}"; do
        copy_local_dep_groups "$source_dir" "$target_dir" "$prefix" "$DEFAULT_LOCAL_ARTIFACT_KEEP"
    done
}

copy_latest_incremental_dirs() {
    local source_dir="$1"
    local target_dir="$2"
    local prefix="$3"
    local keep="$4"
    local dirs=()
    local dir

    if [[ ! -d "$source_dir" ]]; then
        return 0
    fi

    mapfile -t dirs < <(
        find "$source_dir" -maxdepth 1 -mindepth 1 -type d -name "${prefix}-*" -printf '%T@ %p\n' |
            sort -nr |
            awk -v keep="$keep" 'NR <= keep { $1=""; sub(/^ /, ""); print }'
    )

    if ((${#dirs[@]} == 0)); then
        info "skip debug/incremental ${prefix}: no matching directories"
        return 0
    fi

    info "seed debug/incremental ${prefix}: keeping latest ${#dirs[@]} director$( (( ${#dirs[@]} == 1 )) && printf 'y' || printf 'ies' )"
    run_cmd mkdir -p "$target_dir"
    for dir in "${dirs[@]}"; do
        run_cmd cp -a "$dir" "$target_dir/"
    done
}

seed_debug_incremental_subset() {
    local source_dir="$1"
    local target_dir="$2"
    local prefix

    if [[ ! -d "$source_dir" ]]; then
        info "skip debug/incremental subset: source missing at $source_dir"
        return 0
    fi

    for prefix in "${INCREMENTAL_PREFIXES[@]}"; do
        copy_latest_incremental_dirs "$source_dir" "$target_dir" "$prefix" "$DEFAULT_INCREMENTAL_KEEP"
    done
}

seed_target_subset() {
    local source_root="$1"
    local target_root="$2"
    local debug_source="$source_root/debug"
    local debug_target="$target_root/debug"
    local file_name

    if [[ ! -d "$debug_source" ]]; then
        info "skip target subset: source debug dir missing at $debug_source"
        return 0
    fi

    info "seed target subset: pruned high-value debug cache"
    seed_debug_deps_subset "$debug_source/deps" "$debug_target/deps"
    sync_tree "$debug_source/build" "$debug_target/build" "target debug/build"
    sync_tree "$debug_source/.fingerprint" "$debug_target/.fingerprint" "target debug/.fingerprint"

    for file_name in "${ROOT_DEBUG_FILES[@]}"; do
        copy_file_if_present "$debug_source/$file_name" "$debug_target/$file_name" "target debug/$file_name"
    done

    if ((WITH_INCREMENTAL)); then
        seed_debug_incremental_subset "$debug_source/incremental" "$debug_target/incremental"
    else
        info "skip target debug/incremental: default warm subset leaves incremental state out"
    fi
}

share_path() {
    local source_path="$1"
    local target_path="$2"
    local label="$3"

    if [[ ! -e "$source_path" ]]; then
        info "skip shared ${label}: source missing at $source_path"
        return 0
    fi

    if [[ -L "$target_path" && "$(readlink "$target_path")" == "$source_path" ]]; then
        info "shared ${label} already points at $source_path"
        return 0
    fi

    if [[ -e "$target_path" || -L "$target_path" ]]; then
        die "refusing to replace existing ${label} at $target_path; remove it or rerun with a fresh worktree"
    fi

    info "share ${label}: $target_path -> $source_path"
    run_cmd mkdir -p "$(dirname "$target_path")"
    run_cmd ln -s "$source_path" "$target_path"
}

write_agent_state() {
    local agent_state_dir="$1"
    local worktree_id="$2"
    local branch_name="$3"
    local worktree_path="$4"
    local source_tree="$5"
    local work_root="$6"
    local shared_static_root="$7"
    local target_cache_mode="$8"
    local target_incremental="$9"

    info "write agent state in $agent_state_dir"
    run_cmd mkdir -p "$agent_state_dir"

    if ((DRY_RUN)); then
        return 0
    fi

    cat >"$agent_state_dir/setup.env" <<EOF
AGENT_WORKTREE_ID="$worktree_id"
AGENT_WORKTREE_BRANCH="$branch_name"
AGENT_WORKTREE_PATH="$worktree_path"
AGENT_SOURCE_TREE="$source_tree"
AGENT_WORK_ROOT="$work_root"
AGENT_WORK_DIR_TEMPLATE="$work_root/<mode>"
RESTREAM_BUILD_LOCK_FILE="$DEFAULT_LOCK_FILE"
AGENT_SHARED_STATIC_ROOT="$shared_static_root"
AGENT_TARGET_CACHE_MODE="$target_cache_mode"
AGENT_TARGET_CACHE_WITH_INCREMENTAL="$target_incremental"
AGENT_WORKTREE_CLEANUP_COMMAND="scripts/agent-worktree.sh --cleanup $worktree_id"
EOF

    cat >"$agent_state_dir/README.txt" <<EOF
This directory belongs to the agent worktree setup helper.

Useful defaults:
  source setup.env
  export WORK_ROOT="$work_root"
  export RESTREAM_BUILD_LOCK_FILE="$DEFAULT_LOCK_FILE"

Live harness examples:
  WORK_DIR="$work_root/mixed-anchor" target/debug/test_harness mixed-anchor
  WORK_ROOT="$work_root/suite" cargo run --bin test_harness -- suite --work-root "$work_root/suite"

Cleanup when finished:
  scripts/agent-worktree.sh --cleanup "$worktree_id"
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --cleanup)
            DO_CLEANUP=1
            shift
            ;;
        --force-cleanup)
            FORCE_CLEANUP=1
            shift
            ;;
        --path)
            [[ $# -ge 2 ]] || die "--path requires a value"
            WORKTREE_PATH="$2"
            shift 2
            ;;
        --branch)
            [[ $# -ge 2 ]] || die "--branch requires a value"
            BRANCH_NAME="$2"
            shift 2
            ;;
        --base)
            [[ $# -ge 2 ]] || die "--base requires a value"
            BASE_REF="$2"
            shift 2
            ;;
        --source)
            [[ $# -ge 2 ]] || die "--source requires a value"
            SOURCE_TREE="$2"
            shift 2
            ;;
        --full-target-cache)
            TARGET_CACHE_MODE="full"
            shift
            ;;
        --with-incremental)
            WITH_INCREMENTAL=1
            shift
            ;;
        --no-target-cache)
            SEED_TARGET=0
            shift
            ;;
        --no-cargo-config)
            SEED_CARGO=0
            shift
            ;;
        --no-node-modules)
            SEED_NODE=0
            shift
            ;;
        --no-share-static)
            SHARE_STATIC=0
            shift
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        --)
            shift
            break
            ;;
        -*)
            die "unknown option: $1"
            ;;
        *)
            if [[ -n "$WORKTREE_ID" ]]; then
                die "unexpected extra argument: $1"
            fi
            WORKTREE_ID="$1"
            shift
            ;;
    esac
done

[[ -n "$WORKTREE_ID" ]] || {
    usage >&2
    exit 2
}

SOURCE_TREE="$(resolve_existing_path "$SOURCE_TREE")"
REPO_COMMON_ROOT="$(resolve_common_repo_root "$SOURCE_TREE")"

if [[ -z "$WORKTREE_PATH" ]]; then
    WORKTREE_PATH="$REPO_COMMON_ROOT/worktrees/$WORKTREE_ID"
else
    WORKTREE_PATH="$(resolve_target_path "$WORKTREE_PATH")"
fi

if [[ -z "$BRANCH_NAME" ]]; then
    BRANCH_NAME="codex/$WORKTREE_ID"
fi

STATIC_SOURCE_ROOT="$SOURCE_TREE/.build/static"
PUBLIC_BIN_SOURCE="$SOURCE_TREE/public/bin"
TARGET_SOURCE="$SOURCE_TREE/target"
CARGO_SOURCE="$SOURCE_TREE/.cargo"
NODE_SOURCE="$SOURCE_TREE/node_modules"

info "source tree: $SOURCE_TREE"
info "common repo root: $REPO_COMMON_ROOT"
info "worktree id: $WORKTREE_ID"
info "destination: $WORKTREE_PATH"
info "branch: $BRANCH_NAME"

if ((DO_CLEANUP)); then
    cleanup_worktree "$SOURCE_TREE" "$WORKTREE_PATH" "$FORCE_CLEANUP"
    cat <<EOF
agent-worktree: cleanup complete
  worktree: $WORKTREE_PATH
  branch kept: $BRANCH_NAME
EOF
    exit 0
fi

WORKTREE_EXISTS=0
if [[ -e "$WORKTREE_PATH" ]]; then
    if git -C "$WORKTREE_PATH" rev-parse --show-toplevel >/dev/null 2>&1; then
        WORKTREE_EXISTS=1
        info "destination worktree already exists; refreshing caches and metadata"
    else
        die "destination path already exists and is not a git worktree: $WORKTREE_PATH"
    fi
fi

if ((WORKTREE_EXISTS == 0)); then
    if git -C "$SOURCE_TREE" show-ref --verify --quiet "refs/heads/$BRANCH_NAME"; then
        info "create worktree from existing branch $BRANCH_NAME"
        run_cmd git -C "$SOURCE_TREE" worktree add "$WORKTREE_PATH" "$BRANCH_NAME"
    else
        info "create worktree from $BASE_REF on new branch $BRANCH_NAME"
        run_cmd git -C "$SOURCE_TREE" worktree add -b "$BRANCH_NAME" "$WORKTREE_PATH" "$BASE_REF"
    fi
fi

if ((SEED_TARGET)); then
    if [[ "$TARGET_CACHE_MODE" == "full" ]]; then
        sync_tree "$TARGET_SOURCE" "$WORKTREE_PATH/target" "target cache"
    else
        seed_target_subset "$TARGET_SOURCE" "$WORKTREE_PATH/target"
    fi
fi

if ((SEED_CARGO)); then
    sync_tree "$CARGO_SOURCE" "$WORKTREE_PATH/.cargo" ".cargo config"
fi

if ((SEED_NODE)); then
    sync_tree "$NODE_SOURCE" "$WORKTREE_PATH/node_modules" "node_modules"
fi

if ((SHARE_STATIC)); then
    share_path "$STATIC_SOURCE_ROOT" "$WORKTREE_PATH/.build/static" ".build/static"
    share_path "$PUBLIC_BIN_SOURCE" "$WORKTREE_PATH/public/bin" "public/bin"
else
    info "skip shared static outputs by request"
fi

AGENT_STATE_DIR="$WORKTREE_PATH/.agent-state"
WORK_ROOT_DEFAULT="$WORKTREE_PATH/test/artifacts/agents/$WORKTREE_ID"
SHARED_STATIC_ROOT=""
if ((SHARE_STATIC)); then
    SHARED_STATIC_ROOT="$STATIC_SOURCE_ROOT"
fi
write_agent_state \
    "$AGENT_STATE_DIR" \
    "$WORKTREE_ID" \
    "$BRANCH_NAME" \
    "$WORKTREE_PATH" \
    "$SOURCE_TREE" \
    "$WORK_ROOT_DEFAULT" \
    "$SHARED_STATIC_ROOT" \
    "$TARGET_CACHE_MODE" \
    "$WITH_INCREMENTAL"

cat <<EOF
agent-worktree: ready
  worktree: $WORKTREE_PATH
  branch: $BRANCH_NAME
  source: $SOURCE_TREE
  work root: $WORK_ROOT_DEFAULT
  lock env: RESTREAM_BUILD_LOCK_FILE=$DEFAULT_LOCK_FILE
  cleanup: scripts/agent-worktree.sh --cleanup $WORKTREE_ID
EOF
