// Stream-key display and validation helpers.
// Pure functions for label normalisation, validation error messages, sorting, and
// masked/escaped rendering. No DOM access — used by both the page controller and tests.
import { escapeHtml, maskSecret } from '../utils.js';

/**
 * Strips non-alphanumeric characters (except spaces, underscores, and hyphens)
 * from a raw label string and trims surrounding whitespace.
 * @param {string|null} rawLabel
 * @returns {string}
 */
export function normalizeStreamKeyLabel(rawLabel) {
    return String(rawLabel || '')
        .trim()
        .replace(/[^a-zA-Z0-9 _-]/g, '');
}

/**
 * Returns a validation error message for a stream key label, or `null` when valid.
 * @param {string} label - Already-normalised label value.
 * @returns {string|null}
 */
export function getStreamKeyLabelError(label) {
    if (!label || label.length === 0) {
        return 'Label is required. Please enter a descriptive name.';
    }
    if (label.length < 2) {
        return 'Label must be at least 2 characters.';
    }
    return null;
}

/**
 * Returns a new array of stream keys sorted alphabetically by label.
 * @param {object[]} keys
 * @returns {object[]}
 */
export function sortStreamKeysByLabel(keys) {
    return [...(keys || [])].sort((a, b) => (a.label || '').localeCompare(b.label || ''));
}

/**
 * Sorts the key list and renders it as an HTML table body string, masking and
 * escaping secrets for safe insertion via `innerHTML`.
 * @param {object[]} keys - Array of stream key records from the API.
 * @returns {{sortedKeys: object[], tableHtml: string}}
 */
export function prepareStreamKeysTable(keys) {
    const sortedKeys = sortStreamKeysByLabel(keys);
    const tableHtml = sortedKeys
        .map(
            (keyRow, index) => `
          <tr>
            <th>${index + 1}</th>
            <td>${escapeHtml(keyRow.label || '')}</td>
            <td>${escapeHtml(maskSecret(keyRow.key))}</td>
            <td>
                <button class="btn btn-accent btn-xs js-copy-key" title="Copy" data-key-index="${index}">📋</button>
                <button class="btn btn-accent btn-xs ml-2 js-edit-key" title="Edit" data-key-index="${index}">✎</button>
                <button class="btn btn-error btn-xs ml-2 js-delete-key" title="Delete" data-key-index="${index}">✖</button>
            </td>
          </tr>`,
        )
        .join('');

    return { sortedKeys, tableHtml };
}