# relay 情境——真 agent 版：任務仍在飛行時交棒。前手 claude 派工後不等
# 回覆，寫 handoff、relay 出後手 claude session 便退位；後手讀 handoff、
# 收下前手從未見過的 reply（workers 與 tasks 原地存活）。從 repo 根目錄：
#   vhs docs/demo/real-relay.tape
# 前置與注意事項同 real-hero.tape；後手 pane 同樣落在 scratch 專案目錄，
# 兩層信任與 allowlist 都沿用。編排最複雜、骰運面最大——錄完必抽幀驗：
# (1) relay 出後手 (2) 前手退位 (3) 後手 read 到 reply 並總結。
Output docs/demo/relay-raw.mp4

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
Type "Delegate a one-line summary of docs/notes.md to a codex worker, but do NOT wait for the reply — hand coordination to a successor session as described in CLAUDE.md, and let the successor collect the reply."
Sleep 1
Enter

# 前手 turn（spawn+send+handoff+relay）~60s＋後手 claude 冷啟動＋讀
# handoff＋await/read＋總結 ~90s；抓寬。
Sleep 330

Sleep 6
