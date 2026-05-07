// Shared Express response helpers used by all route modules.
// Centralises the JSON envelope format so route handlers never set status codes or shape
// error payloads directly. respondErrorFromErr handles typed errors from createHttpError.

const { errMsg } = require('./utils');

function respondJson(res, payload, status = 200) {
    if (status === 200) return res.json(payload);
    return res.status(status).json(payload);
}

function respondError(res, status, error, extra = null) {
    return respondJson(res, extra ? { error, ...extra } : { error }, status);
}

function respondEmpty(res, status = 200, headers = null) {
    if (headers) {
        for (const [name, value] of Object.entries(headers)) {
            if (value !== undefined && value !== null) {
                res.set(name, value);
            }
        }
    }
    return res.status(status).end();
}

function buildErrorResponse(err, fallbackStatus = 500) {
    const status = Number(err?.status || fallbackStatus);
    const payload = {
        error: err?.publicError || errMsg(err),
    };

    if (Object.prototype.hasOwnProperty.call(err || {}, 'detail')) {
        payload.detail = err.detail;
    }
    if (Object.prototype.hasOwnProperty.call(err || {}, 'job')) {
        payload.job = err.job;
    }
    if (Object.prototype.hasOwnProperty.call(err || {}, 'logs')) {
        payload.logs = err.logs;
    }

    return { status, payload };
}

function respondErrorFromErr(res, err, fallbackStatus = 500) {
    const { status, payload } = buildErrorResponse(err, fallbackStatus);
    return respondJson(res, payload, status);
}

module.exports = {
    buildErrorResponse,
    respondEmpty,
    respondError,
    respondErrorFromErr,
    respondJson,
};