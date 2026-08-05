# README「獨立 worker 審查」情境 GIF。從 repo 根目錄執行：
#   vhs docs/demo/review.tape
# 依賴：vhs、ttyd、ffmpeg、tmux、jq。全程走 demo stub，不打真實 API。
# 賣點：跨廠牌審查回合——本 session 主導，codex pane 獨立審、回 findings，
# 結論落成可留檔的 task record。
Output docs/assets/review.gif

Set Shell bash
Set FontSize 15
Set Width 1300
Set Height 720
Set Padding 12
Set TypingSpeed 40ms

Hide
Type "./docs/demo/setup.sh docs/demo/review-driver.sh && tmux -L ab-demo attach"
Enter
Sleep 4
Show

Sleep 1
Type "# cross-vendor review round: this session orchestrates, a codex pane audits"
Enter
Sleep 1.2
Type "agent-bridge spawn reviewer --runtime codex"
Enter
Sleep 5

Type "id=$(agent-bridge send reviewer --from main --message-file - <<'EOF'"
Enter
Type "Independent review round on the state-machine diff."
Enter
Type "Hunt for real regressions only — no style nits."
Enter
Type "Reply with CONFIRMED / PLAUSIBLE findings, or 'clean'."
Enter
Type "EOF"
Enter
Type ")"
Enter
Sleep 12

Type "# the verdict is a durable task record — follow-ups go to the same pane"
Enter
Sleep 2
Type "agent-bridge despawn reviewer"
Enter
Sleep 3
