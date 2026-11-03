# Demo GIF for the README. Requires https://github.com/charmbracelet/vhs and a built kvguard on PATH.
Output demo/kvguard.gif
Set Shell "bash"
Set FontSize 15
Set Width 1300
Set Height 720
Set Theme "Catppuccin Mocha"

Type "# A compaction step rewrote an old assistant message. Which turn broke the cache?"
Enter
Sleep 1s
Type "kvguard check tests/fixtures/history-mutation/turn-*.json --fail-on-bust"
Enter
Sleep 4s
Type "echo exit=$?"
Enter
Sleep 3s
