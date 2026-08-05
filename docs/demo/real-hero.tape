# hero demo 的 VHS 劇本——真 agent 版。從 repo 根目錄執行：
#   vhs docs/demo/real-hero.tape
# 與 stub 版 demo.tape 不同：orchestrator 是真 Claude Code、worker 是真
# codex，全程打真實 API。錄影只打一句人話 prompt，其餘畫面全是兩個 agent
# 自己的動作。寬鬆錄、後製壓：raw mp4 交給 real-post.sh 摺疊靜止幀。
# 前置：先跑過一次 real-setup.sh + claude 並接受資料夾信任框（以路徑記
# 錄、跨次有效），錄影中不得出現任何對話框。
# vhs parser 不接受絕對路徑；raw 落在 repo 內（已 gitignore），成品由
# real-post.sh 產出到 docs/assets/。raw 於逐格驗收「之後」才手動刪——
# 驗收前留著可免重錄改後製參數
Output docs/demo/hero-raw.mp4

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
# claude TUI 冷啟動
Sleep 12
Show

Sleep 2
Type "Ask a codex worker to read docs/notes.md and reply with a one-line summary, then report the result back to me."
Sleep 1
Enter

# 整輪真實流程：spawn（codex pane 右側彈出）→ send → await → 通知 → read
# → 總結。彩排實測好的一輪約 70s（task 46s 完成、claude 總結到 ~65s），
# 但 agent 有慢輪（探索、worker 磨蹭）；取 3 倍吸收變異，多出的靜止尾巴
# 由 mpdecimate 摺掉。錄完務必抽幀驗證有結尾（read 回讀＋總結入鏡）。
Sleep 210

# 收尾停留：讓總結畫面站穩幾秒再收
Sleep 6
