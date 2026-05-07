const { EventEmitter } = require('node:events');

function buildProcessMock({ pid = process.pid } = {}) {
    const proc = new EventEmitter();
    proc.pid = pid;
    proc.kills = [];
    proc.kill = (signal) => {
        proc.kills.push(signal);
        return true;
    };
    proc.emitExit = (code = 0, signal = null) => {
        proc.emit('exit', code, signal);
    };
    return proc;
}

module.exports = { buildProcessMock };