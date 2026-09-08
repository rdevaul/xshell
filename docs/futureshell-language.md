# FutureShell language reference

**Status:** FS0 normative draft; syntax is not yet implemented

**Scope:** Minimal deterministic local language plus reserved semantics for
later agent and remote tasks. The [roadmap](futureshell-roadmap.md) remains the
canonical product plan.

## 1. Source and identity

UTF-8 source uses the `.fsh` suffix. Executable files begin with
`#!/usr/bin/env xshell-run`; `#!xshell` is accepted only when a source file is
passed explicitly. A UTF-8 BOM is invalid. Line endings normalize to LF before
source hashing; all other bytes, including trailing whitespace, remain
significant to the source identity.

Identifiers are ASCII `[A-Za-z_][A-Za-z0-9_]*`. Keywords are lowercase and
reserved. Unicode is permitted in strings but not identifiers. `#` begins a
comment outside a string and continues through the line ending.

Imports are workspace-relative UTF-8 paths with `/` separators. Absolute paths,
`.` and `..` components, symlink escape, import cycles, and duplicate module
identities are errors. Planning hashes normalized source bytes for every module
and records the complete ordered module identity set. Imports never execute
code, load native plugins, contact providers, or search ambient paths.

## 2. Lexical forms

- integers are base-10 signed 64-bit values without separators;
- durations are positive integers followed by `ms`, `s`, `m`, or `h`;
- sizes are positive integers followed by `B`, `KiB`, `MiB`, or `GiB`;
- ordinary strings use JSON escapes and may interpolate `${identifier}`;
- raw strings use `r"..."` and do not escape or interpolate;
- paths use `path "relative/name"`; external paths require
  `external_path "/explicit/path"` and an external-path capability;
- lists are homogeneous; maps have string keys and homogeneous values.

Interpolation converts only values with an explicit string representation.
There is no word splitting, glob expansion, command substitution, implicit
environment lookup, or implicit string/path/command conversion.

## 3. Initial types

Scalar types are `bool`, `int`, `string`, `duration`, `size`, `path`, and
`external_path`. Composite types are `list<T>`, `map<T>`, `artifact<T>`,
`task<T>`, and `result<T>`. Runtime handles include `evidence`, `changeset`,
`contract_result`, and `receipt`; they cannot be constructed from strings.

Bindings use `let`, are immutable, and require initialization. Shadowing in the
same lexical scope is invalid. Conditions require `bool`; there are no truthy
conversions. Numeric overflow, invalid unit conversion, and access to an absent
optional value are errors rather than wrapping or coercing.

## 4. Core grammar

This EBNF fixes the FS0 deterministic subset. Whitespace and comments may occur
between tokens. Punctuation inside `shell` is handled by the shell-body scanner.

```ebnf
module       = shebang? , { import | declaration | statement } ;
shebang      = "#!" , text-to-line-end ;
import       = "import" , string , ";" ;
declaration  = contract-declaration ;
statement    = let-statement | transaction | expression , ";" ;
let-statement = "let" , identifier , (":" , type)? , "=" , expression , ";" ;
expression   = literal | identifier | run | shell | agent | verify
             | spawn | await | call ;
run          = "run" , list , task-options ;
shell        = "shell" , shell-options? , "{" , shell-body , "}" , task-options ;
shell-options = "(" , "profile" , ":" , string , ")" ;
verify       = "verify" , "schema" , string , "for" , path-expression , task-options ;
agent        = "agent" , "{" , target-field , connector-field ,
               lifecycle-field , state-owner-field , gateway-field ,
               policy-field , model-field? , prompt-field , context-field? ,
               autonomy , resources , capabilities , "}" ;
target-field = "target" , ":" , string , ";" ;
connector-field = "connector" , ":" , string , ";" ;
lifecycle-field = "lifecycle" , ":" ,
                  ("one_shot" | "managed" | "persistent") , ";" ;
state-owner-field = "state_owner" , ":" ,
                    ("xshell" | "connector" | "external") , ";" ;
gateway-field = "gateway" , ":" ,
                ("coordinator" | "execution_host") , ";" ;
policy-field = "policy" , ":" , string , ";" ;
model-field  = "model" , ":" , string , ";" ;
prompt-field = "prompt" , ":" , string , ";" ;
context-field = "context" , ":" , list , ";" ;
autonomy     = "autonomy" , "{" , { autonomy-field } , "}" ;
autonomy-field = identifier , ":" , literal , ";" ;
spawn        = "spawn" , expression ;
await        = "await" , expression ;
transaction  = "transaction" , identifier , "in" , workspace , block ;
workspace    = "workspace" , "(" , path-expression , ")" ;
block        = "{" , { statement | checkpoint | contract-declaration } , "}" ;
checkpoint   = "checkpoint" , identifier , ";" ;
contract-declaration = "contract" , identifier , "{" ,
                 { require-clause } , valid-handler , invalid-handler , "}" ;
require-clause = "require" , call , ";" ;
valid-handler = "on" , "valid" , promotion-block ;
invalid-handler = "on" , "invalid" , discard-block ;
promotion-block = "{" , "promote" , path-list , ";" , "}" ;
discard-block = "{" , "discard" , ("all" | path-list) , ";" , "}" ;
task-options = "{" , resources , capabilities , "}" ;
resources    = "resources" , "{" , { resource-field } , "}" ;
resource-field = identifier , ":" , (duration | size | integer) , ";" ;
capabilities = "capabilities" , "{" , { capability-field } , "}" ;
capability-field = identifier , ":" , (list | "none") , ";" ;
call         = qualified-identifier , "(" , arguments? , ")" ;
arguments    = argument , { "," , argument } , ","? ;
argument     = identifier , ":" , expression ;
qualified-identifier = identifier , { "." , identifier } ;
list         = "[" , (expression , { "," , expression } , ","?)? , "]" ;
path-list    = list ;
path-expression = "path" , string | "external_path" , string ;
literal      = string | integer | duration | size | path-expression
             | "true" | "false" ;
type         = identifier , ("<" , type , ">")? ;
```

The productions above specify structure, not all semantic names. FS0 recognizes
the resource fields `timeout`, `cpu`, `memory`, `processes`, `output`,
`artifacts`, `retries`, `input_tokens`, `output_tokens`, and `cost_microunits`,
and the capability fields `read`, `write`, `external_read`, `external_write`,
`execute`, `network`, `credentials`, and `devices`; unknown fields are errors.
A task must set positive `timeout` and `output` bounds. Path arguments inside
capability lists use workspace-relative string form because their field
supplies the path type. Trailing commas are permitted in lists and calls;
semicolons terminate fields and clauses.

The initial autonomy field names are `model_rounds`, `tool_dispatches`,
`timeout`, `input_tokens`, `output_tokens`, `cost_microunits`, `currency`, and
`on_exhausted`. The last is one of `stop`, `fail`, or `request_approval` written
as a string. The lexical nonterminals `identifier`, `string`, `integer`,
`duration`, and `size` are defined in sections 1–2.

### 4.1 Program interfaces and Flow lowering

The initial file has one implicit `main` entry point. Its typed input/output
interface is supplied by a pinned program manifest during resolution rather
than inferred from filesystem behavior. Imports contribute declarations but do
not create ambient entry points. A call must match the manifest's port names
and types exactly, and the manifest's required capabilities must fit the call
site envelope. In-source exported-entrypoint syntax is deferred until a real
multi-program fixture requires it.

Lowering creates one Flow task node for each `run`, `shell`, `verify`, `agent`,
or resolved program call; `spawn`/`await` create dependency edges;
transactions create lexical regions; and contracts create deterministic route
gates plus a separate promotion policy. Source order or stable explicit names
derive task IDs—presentation layout and task completion order never do. The
FEA source and its pinned `programs.json` manifest are the first text-to-Flow
review pair; FS1 will add the golden Flow and Plan snapshots produced by the
parser rather than hand-authoring those outputs now.

Functions, bounded iteration, remote placement, and typed xshell service calls
are reserved for later grammar increments. They cannot be accepted as
implementation-defined extensions by an FS1 parser. Agent syntax is fixed for
planning and review in FS0, but execution does not enter the runtime until FS5.

## 5. Process execution

`run [program, arg...]` is argv-native. The list must be nonempty and every item
must be a string or explicitly stringified value. It performs no shell parsing.
Planning resolves the executable allowlist and records requested capabilities;
runtime resolution records the executable path and content identity.

`shell { ... }` evaluates one command string using an explicitly selected shell
profile. It never reads interactive startup files by default. The plan and
receipt identify the shell, body hash, and interpolated values. Shell syntax is
opaque to the FutureShell checker, so shell tasks require a visibly higher
authorization class than equivalent argv-native tasks.

Every task declares positive resource bounds and explicit read, write, execute,
network, credential, and device capabilities. Omitted capability families mean
none. Runtime values may narrow but never expand the statically resolved
envelope.

Capabilities compose by intersection. A module or enclosing transaction may
set a ceiling, a called program declares its requirements, and a task site
grants an envelope; planning succeeds only when the callee requirements are a
subset of both enclosing ceiling and task grant. Child tasks inherit no new
authority from a parent, and imported modules never inherit ambient caller
credentials or paths. Empty and omitted capability families both mean none in
source; normalized plans spell out every family.

## 6. Structured concurrency

`spawn` creates a child owned by the current lexical scope; `await` consumes its
task handle and yields a result. Scope exit requires every child to have been
awaited or cancelled. Detached work is not part of the FS0 subset and, when
introduced, cannot retain transactional write capabilities.

Independent results are ordered by plan task identity, not completion time.
Concurrent tasks with potentially overlapping writes are rejected unless an
explicit future serialization policy orders them. Cancellation propagates to
children and must terminate complete process groups before transaction exit.

## 7. Transactions and checkpoints

A transaction names a declared workspace and creates a private staged view.
The destination is not modified during task execution. A filesystem checkpoint
names a state within that staged transaction; it is unrelated to a signed audit
checkpoint. Handles cannot cross their transaction's lifetime.

`promote` applies only enumerated paths accepted by a valid contract and policy.
`discard all` releases staged state without changing the destination. Promotion
detects concurrent destination changes and is crash-recoverable, not an atomic
multi-file commit. Effects outside the staged workspace are taints, not
rollback-safe changes.

## 8. Contracts

Contracts are pure boolean expression trees over a closed evidence bundle,
final change set, verifier results, and trusted identities. `require` clauses
are evaluated without filesystem, process, network, clock, randomness,
environment, or model access. Each clause returns `pass`, `fail`, or `error`
with referenced evidence IDs and bounded diagnostics.

Hard predicates come only from the versioned predicate catalog. Natural-language
or model review is advisory evidence and cannot satisfy a hard predicate.
Contract validity is necessary but not sufficient for promotion: authorization,
assurance, taint, declared-output, and destination-conflict policy also apply.

## 9. Agent and remote semantics

Agent declarations pin a target/profile, connector, lifecycle,
state owner, gateway location and policy, prompt/context boundary, capabilities,
and model-round/tool/time/token/cost/output/artifact budgets. Persistent state
mutation is an external taint unless a verified connector checkpoint protects
it. Unmediated native tools are ambient authority and may be restricted to
advisory work.

Remote hosts reauthorize their plan fragments and own independent transactions.
Receipts and promotion decisions are per host; FutureShell does not promise
distributed atomicity.

An agent block has the same structured task lifecycle as `run`: planned,
authorized, started, terminal, and awaited or cancelled. `one_shot` state is
disposed at task end; `managed` state has explicit xshell ownership and
retention; `persistent` state is externally owned and is detached, never
silently destroyed. Agent execution is unavailable unless connector
capabilities satisfy every requested lifecycle, mediation, accounting, and
checkpoint requirement. Model output and connector claims are advisory unless
a trusted gateway observes the corresponding action.

## 10. Determinism and nondeterminism

Parsing, formatting, type checking, Flow lowering, catalog resolution,
capability normalization, plan construction, contract evaluation, and receipt
verification are deterministic for identical declared inputs. Plan identity
excludes presentation metadata and includes normalized module, program,
predicate, capability, resource, transaction, and contract identities.

Process scheduling, clocks, random data, environment values, filesystem state,
network responses, provider/model output, remote state, and user approvals are
nondeterministic inputs. They must be prohibited, pinned, or recorded as typed
evidence; none may silently influence static authorization.

## 11. Failures and exit status

Diagnostics have stable codes and source spans; wording is not a compatibility
surface. The checker collects independent errors within configured limits.
Initial `xshell-run` exit codes are: `0` success, `2` source/check/plan error,
`3` authorization failure, `4` task or contract failure, `5` promotion conflict
or recovery required, `6` receipt verification failure, and `70` internal
error. Cancellation maps to `130` for an interactive interrupt and `4` when
reported as a workflow result.

`check`, `fmt`, and `plan` must not execute programs, contact providers, or read
undeclared project files. Malformed input must not cause unbounded allocation,
panic, or fallback to shell interpretation.

## 12. Conformance corpus

`fixtures/futureshell/syntax/valid` contains canonical source. Each invalid
fixture has an adjacent JSON expectation containing stable diagnostic codes and
spans. Until the FS1 parser exists these are specification fixtures, not passing
parser tests. The deterministic FEA fixture is the accepted FS0 vertical-slice
source and intentionally uses fake tools rather than gmsh or FEniCS.
