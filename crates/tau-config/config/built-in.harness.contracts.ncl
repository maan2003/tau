# Schema/default metadata and prompt-template helpers for the built-in harness
# config. `tau-config` embeds this file and places it in scope as
# `harnessContracts` before evaluating `built-in.harness.ncl`; user config files
# do not import or repeat these contracts.
let PromptSection = {
    order | Number,
    text | String,
    enabled | Bool | default = true,
    ..
} in
let SkillPrompt = {
    name | String,
    description | String,
} in
let escapeXml = fun text =>
    text
    |> std.string.replace "&" "&amp;"
    |> std.string.replace "<" "&lt;"
    |> std.string.replace ">" "&gt;"
    |> std.string.replace "\"" "&quot;"
    |> std.string.replace "'" "&apos;" in
let renderPromptSections = fun sections =>
    sections
    |> std.record.to_array
    |> std.array.sort (fun left right =>
        let byOrder = std.number.compare left.value.order right.value.order in
        if byOrder == 'Equal then
            std.string.compare left.field right.field
        else
            byOrder)
    |> std.array.filter (fun section => section.value.enabled)
    |> std.array.map (fun section => section.value.text)
    |> std.array.filter (fun text => text != "")
    |> std.string.join "\n\n" in
let renderSkillsSection = fun skills =>
    if std.array.length skills == 0 then
        ""
    else
        let renderedSkills =
            skills
            |> std.array.map (fun skill =>
                "  <skill>\n    <name>%{escapeXml skill.name}</name>\n    <description>%{escapeXml skill.description}</description>\n  </skill>")
            |> std.string.join "\n" in
        "Skills provide specialized instructions for specific tasks.\nBelow are skills you should be initially aware of. Use the skill tool to load a skill, and search for more skills.\n\n<available_skills>\n%{renderedSkills}\n</available_skills>" in
let SystemPromptTemplate = {
    roleName | String,
    cwd | String,
    skills | Array SkillPrompt,
    toolPromptHooksText | String,
    sections | { _ : PromptSection },
    text | String,
    ..
} in
let baseSystemPromptTemplate = {
    roleName | String,
    cwd | String,
    skills | Array SkillPrompt,
    toolPromptHooksText | String,
    sections | { _ : PromptSection } = {
        intro.order | default = 10,
        intro.text | default = m%"
You are an expert coding assistant operating inside Tau, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.

You can call multiple tools in a single response. If you intend to call multiple tools and there are no dependencies between the calls, make all independent tool calls in parallel in the same response. Maximize use of parallel tool calls where possible to increase efficiency. However, if some tool calls depend on previous calls to inform dependent values, do NOT call these tools in parallel and instead call them sequentially. For instance, if one operation must complete before another starts, run these operations sequentially instead.
"%,
        dedup.order | default = 20,
        dedup.text | default = "Tau deduplicates tool result outputs. `[tau-dedup]` results are not errors, but an optimization pointing at an earlier identical result.",
        availableSkills.order | default = 30,
        availableSkills.text | default = renderSkillsSection skills,
        currentWorkingDirectory.order | default = 40,
        currentWorkingDirectory.text | default = "Current working directory: %{cwd}",
        toolPromptHooks.order | default = 70,
        toolPromptHooks.text | default =
            if toolPromptHooksText == "" then
                ""
            else
                toolPromptHooksText,
    },
    text | default = renderPromptSections sections,
} in
let BuiltInRole = {
    description | String,
    systemPrompt | not_exported | SystemPromptTemplate = baseSystemPromptTemplate,
    ..
} in
let BuiltInHarnessConfigContract = {
    roles | {
        smart | BuiltInRole,
        rush | BuiltInRole,
        deep | BuiltInRole,
        foreman | BuiltInRole,
        ..
    },
    ..
} in
{
    BuiltInHarnessConfig = BuiltInHarnessConfigContract,
    renderSubTaskRoleList = fun roles roleNames =>
        roleNames
        |> std.array.map (fun roleName =>
            let role = std.record.get_or roleName {} roles in
            let description = std.record.get_or "description" roleName role in
            "* `%{roleName}` - \"%{description}\"")
        |> std.string.join "\n",
}
