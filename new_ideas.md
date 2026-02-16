let's try to simplify agent loop usability for an human

what if we do agent-loop plan tasks implement "i want an to create an marketing website for X"

the idea is that it will do the plan.md, decompose in tasks and then do the implementation with the reviewing we are already supporting

if we do agent-loop plan-from="filepath" tasks implement it should generate the tasks and implement them

if we do -single-agent="codex" we should already assume it's codex only or claude for example

i'm trying to get some simplification in the commands for me not to forget about them, currently i think they are a bit confusing and i keep forgetting

if by any chance this stops we need to be able to pickup where we were

we need to be able to have this not deleting the tasks, only if we do -tasks again, so an resume should be able to exist
