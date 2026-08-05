# README「relay 交棒」情境 GIF。從 repo 根目錄執行：
#   vhs docs/demo/relay.tape
# 依賴：vhs、ttyd、ffmpeg、tmux、jq。全程走 demo stub，不打真實 API。
# 賣點：主導 session 的 context 吃緊時，派工不等結果、寫交接檔（含 in-flight
# task id）、relay 給接棒 session；task 跨越交棒仍在跑，接手者查 status、
# 收割前任派出的回覆——worker 與任務都原地活過交棒。
Output docs/assets/relay.gif

Set Shell bash
Set FontSize 15
Set Width 1300
Set Height 720
Set Padding 12
Set TypingSpeed 40ms

Hide
Type "./docs/demo/setup.sh docs/demo/relay-driver.sh && tmux -L ab-demo attach"
Enter
Sleep 4
Show

Sleep 1
Type "# a worker holds project context; dispatch a task without waiting"
Enter
Sleep 1.2
Type "agent-bridge spawn researcher --runtime codex"
Enter
Sleep 5
Type "id=$(agent-bridge send researcher --from main --message-file - <<'EOF'"
Enter
Type "Collect the final perf numbers for the changelog."
Enter
Type "Reply with the summary line only."
Enter
Type "EOF"
Enter
Type ")"
Enter
Sleep 3

Type "# context running tight — hand leadership forward, task still in flight"
Enter
Sleep 1.5
Type "cat > /tmp/agent-bridge-demo/handoff.md <<EOF"
Enter
Type "# handoff — release prep"
Enter
Type "In-flight: task $id (researcher, perf numbers)."
Enter
Type "Next: read that reply, then draft the changelog."
Enter
Type "EOF"
Enter
Sleep 1.5

Type "agent-bridge relay successor --runtime codex --no-select --handoff /tmp/agent-bridge-demo/handoff.md"
Enter
Sleep 5.5
Type "# leadership handed off — watch the successor take over:"
Enter
Sleep 2
Ctrl+B
Type "n"
Sleep 19
