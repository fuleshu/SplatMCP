# Adashi Rule Injection

If the Adashi MCP server is available in this workspace, use it for rule injection and memory.

The project parameter is Aworkit. Get the memory rule and use the memory api accordingly.
Before starting work on a user request, classify the request intend as exactly one of:

- `general`: discussion, explanation, investigation, or operational help where no design deliverable or code edit is expected.
- `design`: architecture, planning, review of an approach, or discussion-only technical design where code should not be changed unless the user explicitly switches to implementation.
- `implementation`: code creation, code modification, tests, builds, migrations, generated files, or any task expected to change the project.

Use these lifecycle hooks:

- `run.start`: before beginning the overall user request.
- `task.start`: before beginning each concrete task in the run. If there is no explicit task list, treat the whole request as one implicit task.
- `task.end`: before marking each concrete task complete.
- `run.end`: before the final response for the overall user request.

At each hook, call the Adashi MCP tool `adashi_get_rule_injections` with:

```json
{
  "intend": "general | design | implementation",
  "hook": "run.start | task.start | task.end | run.end"
}
```

If the MCP call returns rules, treat the returned `injectionPrompt` as active instructions for that hook. Apply the injected prompt before continuing. If the call returns no rules, continue normally.

For multi-task requests, call `task.start` and `task.end` for each task using the same run-level intend unless the user clearly changes the nature of a specific task. Do not invent new intend or hook names.

If the Adashi MCP server is not present, unavailable, or the tool call fails because the MCP surface is not configured, continue without Adashi rule injection and mention the limitation only when it affects the requested outcome.
