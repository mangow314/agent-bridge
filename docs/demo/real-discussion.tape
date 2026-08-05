# discussion 情境——真 agent 版：同一個 worker 兩輪問答，第二輪不重新
# briefing（worker 的脈絡活在自己的 pane）。從 repo 根目錄執行：
#   vhs docs/demo/real-discussion.tape
# 前置與注意事項同 real-hero.tape（兩層信任已持久化；錄完必抽幀驗結尾）。
Output docs/demo/discussion-raw.mp4

Set Shell bash
Set FontFamily "FiraCode Nerd Font Mono"
Set FontSize 15
Set Width 1300
Set Height 720
Set Padding 12
Set TypingSpeed 40ms
Set Framerate 15

Hide
Type "./docs/demo/real-setup.sh && tmux -L ab-real attach"
Enter
Sleep 12
Show

Sleep 2
Type "Ask a codex worker for a one-line summary of docs/notes.md. When the answer comes back, send the SAME worker one follow-up question — which of those changes matters most to users? — and report both answers."
Sleep 1
Enter

# 兩輪：spawn→send→read→再 send（同 worker、無 spawn）→read→總結。
# hero 單輪實測 ~70s，兩輪抓 2 倍再加變異餘裕。
Sleep 300

Sleep 6
