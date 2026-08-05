# README「獨立 worker 討論」情境 GIF。從 repo 根目錄執行：
#   vhs docs/demo/discussion.tape
# 依賴：vhs、ttyd、ffmpeg、tmux、jq。全程走 demo stub，不打真實 API。
# 賣點：同一個 worker pane 連吃兩輪任務，第二輪不必重講前情——脈絡留在 pane。
Output docs/assets/discussion.gif

Set Shell bash
Set FontSize 15
Set Width 1300
Set Height 720
Set Padding 12
Set TypingSpeed 40ms

Hide
Type "./docs/demo/setup.sh docs/demo/discussion-driver.sh && tmux -L ab-demo attach"
Enter
Sleep 4
Show

Sleep 1
Type "# a worker pane keeps its context — use it for multi-round discussion"
Enter
Sleep 1.2
Type "agent-bridge spawn advisor --runtime codex"
Enter
Sleep 5

Type "id=$(agent-bridge send advisor --from main --message-file - <<'EOF'"
Enter
Type "Design question: keep the timestamp prefix in task ids?"
Enter
Type "State a position and one risk."
Enter
Type "EOF"
Enter
Type ")"
Enter
Sleep 10.5

Type "# follow-up to the same pane — round 1 is still in its context"
Enter
Sleep 1.5
Type "id=$(agent-bridge send advisor --from main --message-file - <<'EOF'"
Enter
Type "Follow-up: ids are now minted on a single host only."
Enter
Type "Does your risk still hold?"
Enter
Type "EOF"
Enter
Type ")"
Enter
Sleep 10.5

Type "agent-bridge despawn advisor"
Enter
Sleep 3
