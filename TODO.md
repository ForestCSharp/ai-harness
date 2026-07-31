
# DO IN ORDER
[x] move sessions under a .ai_harness dir for cwd when using harness. so sessions would be under .ai_harness/sessions and each session gets its own folder (where plan files and other session-specific will eventually be stored)
[x] <ai-harness-option> <ai-harness-option-choice>a</ai-harness-option-choice> ... </ai-harness-option> to allow the LLM to ask follow-up questions
[x] support for displaying markdown output for eventual plan mode but also general LLM output
[x] /plan mode that writes a plan to .ai_harness/, asks follow up questions, and eventually prompts to execute the plan, which will leave plan mode and start working
    During plan mode, only writes to the plan.MD in the sessions directory file should be allowed 
