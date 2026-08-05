# review 情境——真 agent 版：跨供應商審查回合。codex worker 讀本 repo 的
# 未提交 diff（setup 植入的 unquoted 展開 bug），回覆裁決；裁決以 durable
# task record 回到 orchestrator。從 repo 根目錄執行：
#   vhs docs/demo/real-review.tape
# 前置與注意事項同 real-hero.tape（兩層信任已持久化；錄完必抽幀驗結尾）。
Output docs/demo/review-raw.mp4

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
Type "Have a codex worker review the uncommitted git diff in this repo and reply with a verdict — real bug or not, and why. Report its verdict back to me."
Sleep 1
Enter

# 單輪但 worker 要跑 git diff＋分析；hero 單輪 ~70s，審查抓 1.5–2 倍。
Sleep 240

Sleep 6
