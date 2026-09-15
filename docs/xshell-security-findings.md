# xshell security finding ledger

**Updated:** 13 September 2026

**Authority:** The evidence and acceptance conditions in the
[design, security, and usability review](xshell-design-security-usability-review.md)
remain authoritative. This page tracks implementation; it does not rewrite the
historical review.

| ID | Tracking | Priority | Package | State | Required regression evidence |
|---|---|---|---|---|---|
| F1 | [#51](https://github.com/rdevaul/xshell/issues/51) | P1 | M1.1 effects and recovery | Corrected | Completed tool effect and terminal failure survive reload and appear in the next request |
| F2 | [#52](https://github.com/rdevaul/xshell/issues/52) | P1 | M1.2 cancellation and shutdown | Corrected | Agent and captured-shell pipelines/grandchildren cannot write delayed markers after cancellation |
| F3 | [#53](https://github.com/rdevaul/xshell/issues/53) | P1 | M1.2 cancellation and shutdown | Corrected | SIGTERM drains agent, shell, and PTY work before audit closure; forced interruption is recovered as uncertain |
| F4 | [#54](https://github.com/rdevaul/xshell/issues/54) | P1 | M1.3 prepared file access | Open | Sensitivity is stable across cwd changes, nesting, aliases, and sensitive ancestors |
| F5 | [#55](https://github.com/rdevaul/xshell/issues/55) | P1 | M1.3 prepared file access | Open | Replacement hooks cannot substitute sensitive or outside-root bytes after authorization |
| F6 | [#56](https://github.com/rdevaul/xshell/issues/56) | P1 | M1.4 service bounds | Open | A non-acknowledging audit peer cannot freeze unrelated sessions, control, or shutdown |
| F7 | [#57](https://github.com/rdevaul/xshell/issues/57) | P2 | M1.1 effects and recovery | Open | Injected write/sync/rename failures leave response, memory, and restarted state consistent |
| F8 | [#58](https://github.com/rdevaul/xshell/issues/58) | P2 | M1.3 prepared file access | Open | File and directory acquisition remain bounded before allocation and control stays responsive |
| F9 | [#59](https://github.com/rdevaul/xshell/issues/59) | P2 | M1.4 provider bounds | Open | Incomplete/error/refusal streams cannot produce executable tool calls |
| F10 | [#60](https://github.com/rdevaul/xshell/issues/60) | P2 | M1.4 resource bounds | Open | Maximal repeated batches stop at deterministic request, tool, byte, time, and worker budgets |
| F11 | [#61](https://github.com/rdevaul/xshell/issues/61) | P2 | M1.5 directory consistency | Corrected | Files cannot become cwd; invalid updates preserve the previous cwd in every mode |

## Evidence rules

- Keep `review-evidence/review_probe.rs` unchanged as historical reproduction
  source from revision `4ca0839`.
- Corrected behavior belongs in active crate or integration tests named with the
  finding ID in a nearby comment or assertion message.
- A finding becomes **Corrected** only when its original reproduction no longer
  demonstrates the defect and the review's acceptance condition is covered.
- A narrower disposition must document the unsupported mode or guarantee and
  explain why it is not silently downgraded.
- Relevant corrections must pass on supported macOS and Linux targets. Process,
  filesystem, and shutdown corrections require review of failure paths, not
  only successful command execution.

## Planned sequence

1. Complete M0 contracts and the mode matrix.
2. Correct F1, F2, F3, and the independent F11 defect.
3. Implement prepared file operations for F4, F5, and F8.
4. Correct audit/provider/resource bounds for F6, F9, and F10.
5. Complete F7 transactional persistence and crash-durability injection.

F3 has graceful SIGTERM coverage for agent tools, captured shell commands, and
PTYs, including descendant-process cleanup and audit-close ordering. A durable
pre-dispatch marker converts SIGKILL leftovers into an explicit
`outcome_unknown` recovery record. Audit-peer responsiveness remains the
separate F6 correction.

The [development roadmap](xshell-development-roadmap.md) remains the
cross-product sequencing authority.
