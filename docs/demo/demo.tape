# README demo GIF 的 VHS 劇本。從 repo 根目錄執行：
#   vhs docs/demo/demo.tape
# 依賴：vhs、ttyd、ffmpeg、tmux、jq。全程走 demo stub，不打真實 API。
Output docs/assets/demo.gif

Set Shell bash
Set FontSize 15
Set Width 1300
Set Height 720
Set Padding 12
Set TypingSpeed 40ms

Hide
Type "./docs/demo/setup.sh && tmux -L ab-demo attach"
Enter
Sleep 4
Show

Sleep 1
Type "# spawn a worker pane (codex stand-in stub for this recording)"
Enter
Sleep 1.2
Type "agent-bridge spawn researcher --runtime codex"
Enter
Sleep 5
Type "agent-bridge list"
Enter
Sleep 2.5

Type "id=$(agent-bridge send researcher --from main --message-file - <<'EOF'"
Enter
Type "Run bash tests/run-tests.sh and report the summary line."
Enter
Type "Read-only: do not modify any files."
Enter
Type "EOF"
Enter
Type ")"
Enter
Sleep 1.5
Type "# reply lands in the mailbox; the bridge notifies this pane automatically"
Enter
Sleep 7.5
Sleep 4

Type "# worker pane keeps its context for follow-ups; reclaim when done:"
Enter
Sleep 1.2
Type "agent-bridge despawn researcher"
Enter
Sleep 2
Type "agent-bridge list"
Enter
Sleep 3
