const express = require('express');

function createExpressHarness(registerRoutes) {
    const app = express();
    app.use(express.json());
    registerRoutes(app);
    return app;
}

module.exports = { createExpressHarness };