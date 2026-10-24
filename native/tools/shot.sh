#!/bin/zsh
# Screenshot only the Herse window: tools/shot.sh out.png [process-name]
set -e
id=$("${0:a:h}/winid" "${2:-herse}")
screencapture -x -o -l"$id" "$1"
