#!/usr/bin/env xshell-run

# Planning fixture only: agent execution is an FS5 capability.
let review = await agent {
    target: "reviewer";
    connector: "openai-compatible";
    lifecycle: one_shot;
    state_owner: xshell;
    gateway: coordinator;
    policy: "strict-review";
    model: "pinned-review-model";
    prompt: "Review the declared analysis artifact.";
    context: ["analysis.json"];
    autonomy {
        model_rounds: 2;
        tool_dispatches: 0;
        timeout: 2m;
        on_exhausted: "fail";
    }
    resources {
        timeout: 2m;
        output: 64KiB;
        input_tokens: 8000;
        output_tokens: 2000;
        cost_microunits: 500000;
    }
    capabilities {
        read: ["analysis.json"];
        write: none;
        external_read: none;
        external_write: none;
        execute: none;
        network: none;
        credentials: none;
        devices: none;
    }
};
