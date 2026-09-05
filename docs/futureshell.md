# Next Generation Xshell
Xshell should become its own scripting language that allows the user to mix deterministic bash-like shell syntax with the output of LLMs, invoked either in-line or as subprocesses. These scripts would be a more powerful and deterministic alternative to the current notion of an LLM skill, which ultimately has zero enforcability.

This would involve the following:
 (1) Extending Xshell with the standard shell # comment syntax
 (2) Employing the shebang #!xshell convention to designate an xshell script
 (3) The invocation of xshell subprocesses with bounded and prescribed resources, running locally or remotely, with the output being redirectable like any shell process STDOUT and STDERR.  There would be an async/await syntax to allow a collection of processes to be kicked off and then the results gathered.  The results of this type of operation would be made conditional on the notion of a CONTRACT, see below, with the ability to roll back state using the CHECKPOINT mechanism described below.
 (4) The concept of a contract, which is a condition that must be true in order for results of previous operations to be accepted. Importantly, contracts can be contingent on audit-tracked operations, not just the LLM promising that something was done, or a file being written. For example, an LLM is told to run an FEA analysis of a part -- did it actually invoke the FEA mesher and then the von mies stress analysis? Or did it just halucinate the results? You can find out by looking at the audit logs. 
 (5) File checkpointing: Wrapping the bash busybox implmenetation to support the concept of automatic backup on write - I declare a checkpoint as an imperitive in a script, and then subsequent to that any file modification done through the bash core commands results in the creation of a checkpointed backup before any write/append/delete/chmod operation takes place.  Then, subsquently I can conditionally accept or reject file modifications subsequent to the checkpoint.  While this doesn't prevent side effects from other commands from modifying files, it would allow for a rich range of shell-based file operations with contained side effects, e.g.: ```
 $# other stuff has happened
 $checkpoint PREFEA # declare checkpoint
 $await exec("fea.xsh") # invoke this xshell script with default resource limitations, it generates file output, including analysis.json
 $success=contract("did run {gmesh, fenix}; did write {analysis.json}",checkpoint=PREFEA, valid="rollback {all} except {analysis.json}",invalid="rollaback {all}")
 ```
At the core of this is the idea of using the audit system to verify that actions were actually taken, and as part of that to be able to rely on whatever shell logic or command output is needed to verify a contract. And the checkpoint/rollback framework means we have a lightweight "undo" option that means we don't have to predict or manage all of the file-system side effects of an LLM-based process.

xshell scripts couold include // commands as well as shell syntax commands and thus could be used to set up groups of connections, etc. For example, there might be a script that would spin up a new docker container, ssh in and install xshell and then create a session
