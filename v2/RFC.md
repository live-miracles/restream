# RFC-0001: MediaMTX-based Streaming Pipeline Management System

## Status

- Status: Draft
- Version: v1
- Authors: Jinrui Yang
- Last Updated: 2026-01-07

## 1. Background & Motivation

The existing video streaming system is based on an Nginx RTMP setup. Over time, several issues have emerged:

- Repository structure is complex and hard to maintain
- Limited extensibility for multi-output streaming
- Operational complexity and lack of clear abstractions
- Low development velocity due to tight coupling

We decided to migrate the streaming core to MediaMTX, which is actively maintained, well-documented, protocol-agnostic, and designed as a lightweight media routing hub.

The goal of this project is to build a simple, maintainable control plane on top of MediaMTX that enables stream path management and outbound streaming pipelines via a web interface.

## 2. Goals (MVP)

### In Scope

- Manage active MediaMTX paths
- Create and manage streaming pipelines
- Support one input path with multiple output destinations
- Start/stop FFmpeg jobs for outbound streaming
- Basic observability (status and media metrics)
- Simple admin authentication (single admin user)
- Configuration-driven system (YAML/JSON)

### Out of Scope (MVP)

- User management / multi-tenant auth
- Recording and VOD playback
- Advanced HA / failover
- Transcoding-heavy workflows

## 3. Design Philosophy

This system follows a clear separation of concerns:

- MediaMTX is responsible for media routing
- FFmpeg is responsible for media processing and pushing
- Our system is responsible for orchestration, lifecycle, and observability

Key principles:

- Control plane ≠ data plane
- Prefer explicit lifecycle states
- Prefer simple, inspectable storage (JSON/YAML)
- Favor correctness and clarity over performance

## 4. Terminology & Concepts

### 4.1 MediaMTX Concepts

- Path  
  A named stream endpoint in MediaMTX (e.g. mystream)

- Publisher  
  A client that publishes media to a path

- Reader  
  A client that consumes media from a path

### 4.2 System Concepts

- Pipeline  
  A business abstraction that connects one input and one or more outputs

- Pipeline Input  
  A MediaMTX path used as the source of a pipeline

- Pipeline Output  
  An external streaming destination (YouTube, Facebook, etc.)

- FFmpeg Job  
  A runtime process created to push a pipeline output

## 5. High-Level Architecture

Web UI -> Backend API -> MediaMTX -> FFmpeg -> External Platforms

## 6. Pipeline Lifecycle

States: CREATED, READY, RUNNING, STOPPED, ERROR

Transitions:
CREATED -> READY -> RUNNING -> STOPPED
RUNNING -> ERROR

## 7. Data Models

### Path

```json
{
    "id": "path_mystream",
    "name": "mystream",
    "status": "ACTIVE"
}
```

### Pipeline

```json
{
    "id": "pipeline_001",
    "name": "yt-main-stream",
    "input": { "path": "mystream" },
    "outputs": [],
    "state": "RUNNING"
}
```

## 8. API Design

Paths:
GET /api/paths
POST /api/paths

Pipelines:
GET /api/pipelines
POST /api/pipelines
POST /api/pipelines/:id/start
POST /api/pipelines/:id/stop

## 9. Observability

Metrics from MediaMTX, FFmpeg stderr, and ffprobe.

## 10. Configuration

System is bootstrapped from YAML or JSON configuration files.

## 11. Security (MVP)

HTTP basic authentication, trusted environment.

## 12. Future Work

- HTTPS and OTP authentication
- Recording and VOD
- Protocol conversion
- Redundancy and failover
