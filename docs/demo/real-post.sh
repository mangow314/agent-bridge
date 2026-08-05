#!/usr/bin/env bash
# 真 agent 錄影後製：raw mp4 → mpdecimate 摺疊近似靜止幀（等 API 的思考
# 期）→ 調色盤兩段式轉 GIF → 抽幀輸出供逐格查證（文案主張不得超出抽幀
# 證據）。TUI 有 spinner／token 計數器在動，幀永遠不完全相同——摺疊靠
# SAD 門檻容忍小區塊變化，門檻用環境變數可調，對真素材迭代校準。
# 用法：real-post.sh <raw.mp4> <out.gif>
set -euo pipefail

RAW="$1"
OUT="$2"
# 中介產物一律進獨立 mktemp workspace（codex 審查 2026-08-05）：不得覆寫
# raw 同層的既有 folded.mp4／frames/。不自動清（抽幀是逐格驗收的證據），
# 路徑印在結尾報告行，驗收完由人清 /tmp。
WORK="$(mktemp -d "${TMPDIR:-/tmp}/ab-demo-post.XXXXXX")"

# mpdecimate 門檻（8x8 block SAD）：hi=單塊大變化即保留；lo/frac=多少比例
# 的塊超過 lo 才算「有動靜」。2026-08-05 對真素材校準：單字元翻動的塊 SAD
# 可達數千，hi 必須推到近上限（64*255=16320）廢掉單塊觸發，全靠 frac——
# 0.5%≈73 塊，spinner＋計時器＋token 計數（~30-50 塊）摺掉、新輸出行
# （100+ 塊）保留。實測 162s raw → 32s。
HI="${DEMO_POST_HI:-16000}"
LO="${DEMO_POST_LO:-1000}"
FRAC="${DEMO_POST_FRAC:-0.005}"
FPS="${DEMO_POST_FPS:-15}"        # 摺疊後幀數固定，時長=幀數/fps：fps 越高越短
SCALE="${DEMO_POST_SCALE:-950}"   # GIF 寬度（原錄 1300；GitHub README 實顯 ~880）
COLORS="${DEMO_POST_COLORS:-128}" # 終端場景實色少，128 色無感差；再壓 size 主力

ffmpeg -hide_banner -loglevel warning -y -i "$RAW" \
  -vf "mpdecimate=hi=$HI:lo=$LO:frac=$FRAC,setpts=N/$FPS/TB" \
  -r "$FPS" -an "$WORK/folded.mp4"

ffmpeg -hide_banner -loglevel warning -y -i "$WORK/folded.mp4" \
  -vf "scale=$SCALE:-1:flags=lanczos,split[a][b];[a]palettegen=stats_mode=diff:max_colors=$COLORS[p];[b][p]paletteuse=dither=none:diff_mode=rectangle" \
  "$OUT"

# 驗收數據：摺疊前後時長、GIF 檔案大小；抽幀到 workspace 的 frames/ 供
# 逐格查證（workspace 為本次 mktemp 新建，無既有內容可誤刪）
dur_raw=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$RAW")
dur_gif=$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$OUT")
mkdir -p "$WORK/frames"
ffmpeg -hide_banner -loglevel warning -y -i "$WORK/folded.mp4" \
  -vf fps=1 "$WORK/frames/f%03d.png"
printf 'raw=%ss folded_gif=%ss size=%s frames=%s workspace=%s\n' \
  "$dur_raw" "$dur_gif" "$(du -h "$OUT" | cut -f1)" \
  "$(ls "$WORK/frames" | wc -l)" "$WORK" >&2
