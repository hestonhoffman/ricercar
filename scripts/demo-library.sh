#!/usr/bin/env bash
# Builds the synthetic demo library used by the headless screenshot tour
# (RICERCAR_SNAPSHOT) and for manual testing: 12 albums, 59 short FLAC tracks
# of sine tones at various rates/depths, generated covers, one .lrc file, a
# 13th album ripped as one FLAC file with a .cue sheet (4 tracks), a 14th
# in DSD64 (two DSF files and one DFF) and a 15th as a SACD image (3 tracks).
#
#   scripts/demo-library.sh <out-dir>
#
# Needs ffmpeg, ImageMagick 7 (`magick`) and cargo (for the DSD files and the SACD image). The album, artist and playlist
# names are referenced by crates/ricercar-ui/src/snapshot.rs: keep them in sync.
set -euo pipefail

D=${1:?usage: $0 <out-dir>}
for bin in ffmpeg magick cargo; do
  command -v "$bin" >/dev/null || { echo "missing: $bin" >&2; exit 1; }
done
rm -rf "$D"
mkdir -p "$D"

plan=$(cat <<'EOF'
Aurelia Stone	Northern Lights	2019	Ambient	44100	16	Aurora|Polar Night|Glass Fjord|Magnetosphere|Tundra Hymn|First Light	0
Aurelia Stone	Salt & Silver	2022	Ambient	96000	24	Tidewater|Salt Flats|Silver Coast|Undertow|Low Tide	1
The Meridian Quartet	Blue Hours	1998	Jazz	96000	24	Blue Hour|Late Set|Brushes|Night Owl|Coda in F|Walking Home	2
The Meridian Quartet	Live at the Harbor	2003	Jazz	44100	16	Harbor Intro|Mooring|Fog Horn Blues|Last Ferry	3
Ensemble Lumen	Fugues & Ricercars	2016	Classical	192000	24	Ricercar a 3|Ricercar a 6|Canon per tonos|Fuga I|Fuga II|Contrapunctus	4	Johann Sebastian Bach
Ensemble Lumen	Winter Sonatas	2020	Classical	96000	24	Sonata I – Allegro|Sonata I – Adagio|Sonata II – Presto|Sonata III – Largo	5	Domenico Scarlatti
Kaito Mori	Neon Rain	2021	Electronic	48000	24	Neon Rain|Shibuya 3AM|Circuit|Afterglow|Station Loop	6
Kaito Mori	Signal	2024	Electronic	44100	16	Carrier|Modulation|Noise Floor|Signal	7
Lena Vargas	Paper Houses	2017	Folk	44100	16	Paper Houses|Kitchen Light|Old Roads|Letters|Wild Thyme|Home Again	8
Lena Vargas	Evergreen	2023	Folk	88200	24	Evergreen|Rivers|Lantern|Hollow Oak	9
Orchestre Clair	Nocturnes	2011	Classical	44100	16	Nocturne No. 1|Nocturne No. 2|Nocturne No. 3|Nocturne No. 4	10	Frédéric Chopin
Soft Machines	Analog Dreams	1994	Electronic	44100	16	Analog Dreams|Tape Hiss|VHS Sunset|Reel to Reel|Rewind	11
EOF
)

n=0
while IFS=$'\t' read -r ar al y g sr b tr idx co; do
  dir="$D/$ar/$al"
  mkdir -p "$dir"
  hue=$(( (idx * 37) % 360 )); hue2=$(( (hue + 140) % 360 ))
  magick -size 600x600 -seed $((idx + 7)) "plasma:hsl($hue,70%,45%)-hsl($hue2,60%,20%)" \
    -blur 0x6 -swirl $((idx * 40)) -fill "rgba(255,255,255,0.85)" -pointsize 44 \
    -gravity southwest -annotate +36+40 "$al" "$dir/cover.jpg" </dev/null
  IFS='|' read -ra titles <<< "$tr"
  t=1
  for title in "${titles[@]}"; do
    f=$(( 180 + (t * 47 + idx * 13) % 500 ))
    fmt=s16; [ "$b" = "24" ] && fmt=s32
    composer=(); [ -n "$co" ] && composer=(-metadata composer="$co")
    ffmpeg -nostdin -loglevel error \
      -f lavfi -i "sine=frequency=$f:sample_rate=$sr:duration=$((20 + (t * 7) % 25))" \
      -af "volume=0.2" -ac 2 -sample_fmt $fmt -bits_per_raw_sample "$b" \
      -metadata title="$title" -metadata artist="$ar" -metadata album_artist="$ar" \
      -metadata album="$al" -metadata date="$y" -metadata genre="$g" -metadata track="$t" \
      "${composer[@]}" "$dir/$(printf %02d $t) $title.flac"
    t=$((t + 1)); n=$((n + 1))
  done
done <<< "$plan"

# One file for the whole album, cut into tracks by its cue sheet. The file
# carries the album tags only; titles come from the sheet.
dir="$D/Hana Ito/Live in One Take"
mkdir -p "$dir"
magick -size 600x600 -seed 21 "plasma:hsl(200,70%,45%)-hsl(340,60%,20%)" \
  -blur 0x6 -swirl 120 -fill "rgba(255,255,255,0.85)" -pointsize 44 \
  -gravity southwest -annotate +36+40 "Live in One Take" "$dir/cover.jpg" </dev/null
ffmpeg -nostdin -loglevel error \
  -f lavfi -i "sine=frequency=262:sample_rate=44100:duration=24" \
  -f lavfi -i "sine=frequency=330:sample_rate=44100:duration=26" \
  -f lavfi -i "sine=frequency=392:sample_rate=44100:duration=20" \
  -f lavfi -i "sine=frequency=523:sample_rate=44100:duration=35" \
  -filter_complex "[0][1][2][3]concat=n=4:v=0:a=1,volume=0.2" -ac 2 -sample_fmt s16 \
  -metadata artist="Hana Ito" -metadata album_artist="Hana Ito" \
  -metadata album="Live in One Take" -metadata date="2015" -metadata genre="Jazz" \
  "$dir/Live in One Take.flac"
printf '%s\r\n' 'REM GENRE Jazz' 'REM DATE 2015' 'REM COMMENT "ricercar demo"' \
  'PERFORMER "Hana Ito"' 'TITLE "Live in One Take"' 'FILE "Live in One Take.wav" WAVE' \
  '  TRACK 01 AUDIO' '    TITLE "Opening"' '    INDEX 01 00:00:00' \
  '  TRACK 02 AUDIO' '    TITLE "Second Wind"' '    INDEX 01 00:24:00' \
  '  TRACK 03 AUDIO' '    TITLE "Café Interlude"' '    INDEX 00 00:50:00' '    INDEX 01 00:52:00' \
  '  TRACK 04 AUDIO' '    TITLE "Long Goodbye"' '    PERFORMER "Hana Ito & Friends"' \
  '    INDEX 01 01:10:00' > "$dir/Live in One Take.cue"
n=$((n + 4))

# DSD64: sigma-delta sine tones written by an example of ricercar-audio.
dir="$D/Ensemble Lumen/Pure Fifths"
mkdir -p "$dir"
magick -size 600x600 -seed 33 "plasma:hsl(45,70%,45%)-hsl(260,60%,20%)" \
  -blur 0x6 -swirl 200 -fill "rgba(255,255,255,0.85)" -pointsize 44 \
  -gravity southwest -annotate +36+40 "Pure Fifths" "$dir/cover.jpg" </dev/null
root=$(cd "$(dirname "$0")/.." && pwd)
t=1
for spec in "220|Open Fifth|dsf" "330|Drone in A|dsf" "440|Pure Tone|dff"; do
  IFS='|' read -r f title ext <<< "$spec"
  cargo run -q --manifest-path "$root/Cargo.toml" -p ricercar-audio --example dsd_tone -- \
    "$dir/$(printf %02d $t) $title.$ext" "$f" 20 \
    title="$title" artist="Ensemble Lumen" albumartist="Ensemble Lumen" \
    album="Pure Fifths" date=2021 genre=Classical tracknumber=$t
  t=$((t + 1)); n=$((n + 1))
done

# A SACD image (ISO, stereo area, DSD64): three tracks in one file.
dir="$D/Trio Meridian/Harbour Lights"
mkdir -p "$dir"
magick -size 600x600 -seed 41 "plasma:hsl(200,60%,35%)-hsl(30,70%,55%)" \
  -blur 0x8 -swirl 160 -fill "rgba(255,255,255,0.85)" -pointsize 44 \
  -gravity southwest -annotate +36+40 "Harbour Lights" "$dir/cover.jpg" </dev/null
cargo run -q --manifest-path "$root/Cargo.toml" -p ricercar-audio --example sacd_image -- \
  "$dir/Harbour Lights.iso" "Harbour Lights" "Trio Meridian" \
  "262:12:Morning Tide" "392:12:Lanterns" "523:12:Last Ferry"
n=$((n + 3))

printf '[00:02.00]First line of a synthetic lyric\n[00:06.00]Second line, a little longer than the first\n[00:10.50]<00:10.50>Third <00:11.20>line\n[00:15.00]\n[00:17.00]The chorus comes back again\n[00:22.00]And fades out slowly\n' \
  > "$D/Kaito Mori/Neon Rain/01 Neon Rain.lrc"
echo "$n tracks in $D"
