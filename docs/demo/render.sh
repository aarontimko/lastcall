#!/usr/bin/env bash
# Encode the frames VHS captured into the README GIF.
#
#   docs/demo/render.sh <demo-dir>
#
# VHS 0.12.0 writes <demo-dir>/frames/ (the tape's `Output frames/`) but its own
# ffmpeg step never runs, so this does what that step would have: overlay the cursor
# frames on the text frames, pad with the theme background, and quantise to a palette.
set -euo pipefail
demo="${1:?demo dir}"
cd "$demo"
ffmpeg -loglevel error -y \
    -r 50 -start_number 1 -i frames/frame-text-%05d.png \
    -r 50 -start_number 1 -i frames/frame-cursor-%05d.png \
    -filter_complex "[0][1]overlay[o];[o]pad=iw+28:ih+28:14:14:color=#1e1e2e,fps=20,split[a][b];[a]palettegen=max_colors=256[p];[b][p]paletteuse=dither=bayer:bayer_scale=5" \
    -loop 0 lastcall.gif
ffprobe -v error -select_streams v:0 -show_entries stream=width,height,nb_frames -of csv=p=0 lastcall.gif
ls -l lastcall.gif | awk '{print $5 " bytes"}'
