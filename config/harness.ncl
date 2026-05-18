{
  # This sample uses plain Nickel records/values on purpose. Tau wraps its
  # built-in harness defaults with schema/default metadata before merging user
  # config, so you normally do not need to copy `| Contract` annotations here.

  # Provider extensions publish the live model list and model metadata at
  # runtime. Harness config owns role defaults and extension settings.
  # Use `/model <role>` to switch and `/role ...` to edit persisted role
  # overrides.

  # Optional role defaults. Built-in roles are merged underneath this map:
  # smart = state-of-the-art default, deep = xhigh/detailed,
  # rush = fast/cheap low reasoning, foreman = orchestration/delegation.
  # Role `description` is shown in `/role ...` completions. The built-in
  # non-exported `systemPrompt` Nickel template also reads these descriptions
  # for the foreman role's configured delegation-role list.
  # roles = {
  #   smart = {
  #     # description = "Balanced coding assistant.",
  #     # model = "chatgpt/gpt-5.5",
  #     # toolsProfile = "read_only",
  #   },
  #   deep = {
  #     # effort = "xhigh",
  #     # thinkingSummary = "detailed",
  #   },
  #   rush = {
  #     # effort = "low",
  #     # thinkingSummary = "off",
  #   },
  # },

  # Optional named per-role tool overlays. Each entry maps tool name to
  # enabled/disabled and overrides that tool's `enabled_by_default` hint.
  # toolsProfiles = {
  #   # Built in by default:
  #   # gpt = {
  #   #   apply_patch = true,
  #   #   edit = false,
  #   #   find = false,
  #   #   grep = false,
  #   #   ls = false,
  #   #   read = false,
  #   #   write = false,
  #   # },
  #   read_only = {
  #     shell = false,
  #     write = false,
  #     edit = false,
  #   },
  # },

  # Optional built-in extension overrides.
  # extensions = {
  #   "core-shell" = { enable = false },
  #   "provider-openai" = { enable = false },
  #   "test-dummy" = { enable = true },
  # },
}
