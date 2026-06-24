. "$PSScriptRoot\..\lib\ai-memory-hook.ps1"
Invoke-AiMemoryHook -Event "user-prompt" -Agent "claude-code" -FetchRecall
exit 0
