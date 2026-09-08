#!/usr/bin/env xshell-run

# The FS0 deterministic language slice.
transaction analysis in workspace(path ".") {
    checkpoint input;

    let mesh = await run ["tools/fake-gmsh", "input/model.step", "mesh.msh"] {
        resources { timeout: 5m; output: 1MiB; }
        capabilities {
            read: ["input/model.step"];
            write: ["mesh.msh"];
            execute: ["tools/fake-gmsh"];
            network: none;
        }
    };

    let solve = await run ["tools/fake-fenics", "mesh.msh", "analysis.json"] {
        resources { timeout: 10m; output: 1MiB; }
        capabilities {
            read: ["mesh.msh"];
            write: ["analysis.json"];
            execute: ["tools/fake-fenics"];
            network: none;
        }
    };

    contract valid_analysis {
        require task.succeeded(task: "mesh");
        require task.succeeded(task: "solve");
        require artifact.matches_schema(
            path: "analysis.json",
            schema: "fea-result-v1",
        );
        require changes.allow_only(paths: ["mesh.msh", "analysis.json"]);
        on valid { promote ["analysis.json"]; }
        on invalid { discard all; }
    }
}
