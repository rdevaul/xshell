#!/usr/bin/env xshell-run

# Independently agent-authored equivalent of the bounded FEA review workflow.
transaction analysis in workspace(path ".") {
    checkpoint input;

    let mesh = await run ["tools/fake-gmsh", "input/model.step", "mesh.msh"] {
        resources { timeout: 5m; output: 1MiB; }
        capabilities {
            read: ["input/model.step"];
            write: ["mesh.msh"];
            external_read: none;
            external_write: none;
            execute: ["tools/fake-gmsh"];
            network: none;
            credentials: none;
            devices: none;
        }
    };

    let solve = await run ["tools/fake-fenics", "mesh.msh", "analysis.json"] {
        resources { timeout: 10m; output: 1MiB; }
        capabilities {
            read: ["mesh.msh"];
            write: ["analysis.json"];
            external_read: none;
            external_write: none;
            execute: ["tools/fake-fenics"];
            network: none;
            credentials: none;
            devices: none;
        }
    };

    contract valid_analysis {
        require task.succeeded(task: "mesh");
        require task.succeeded(task: "solve");
        require artifact.matches_schema(path: "analysis.json", schema: "fea-result-v1");
        require changes.allow_only(paths: ["mesh.msh", "analysis.json"]);
        on valid { promote ["analysis.json"]; }
        on invalid { discard all; }
    }
}
