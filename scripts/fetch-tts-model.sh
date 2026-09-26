#!/usr/bin/env bash
# Fetch the Kokoro-82M v1.0 ONNX model and its voice style vectors for se-tts (PLAN §14.4).
#
# Weights: Apache-2.0 (hexgrad/Kokoro-82M, ONNX export by onnx-community). Every file is
# pinned to one Hugging Face revision and verified against the sha256 below, so a moved or
# tampered upstream file fails loudly instead of loading. Idempotent: files that already
# verify are left alone; partial/corrupt files are re-downloaded.
#
# Usage: fetch-tts-model.sh [--dir DIR] [--model FILE] [--all-models]
#   --dir DIR      destination (default: ${XDG_DATA_HOME:-~/.local/share}/stream-engine/models/kokoro)
#   --model FILE   model variant to fetch (default: model_quantized.onnx; any file pinned below)
#   --all-models   fetch every pinned model variant (benchmarking)
# Exit status: 0 = everything present and verified; 1 = download or checksum failure; 2 = usage.
set -euo pipefail

REPO="onnx-community/Kokoro-82M-v1.0-ONNX"
REV="1939ad2a8e416c0acfeecc08a694d14ef25f2231"
BASE="https://huggingface.co/${REPO}/resolve/${REV}"
MODEL_DEFAULT="model_quantized.onnx"

# sha256 of every file at ${REV} (equal to the Hugging Face LFS object ids).
declare -A SUMS=(
  [onnx/model.onnx]=8fbea51ea711f2af382e88c833d9e288c6dc82ce5e98421ea61c058ce21a34cb
  [onnx/model_fp16.onnx]=ba4527a874b42b21e35f468c10d326fdff3c7fc8cac1f85e9eb6c0dfc35c334a
  [onnx/model_q4.onnx]=04cf570cf9c4153694f76347ed4b9a48c1b59ff1de0999e6605d123966b197c7
  [onnx/model_q4f16.onnx]=d1a508a6a29671ead84fac99c7401fbd3c21a583fc6ed1406d1ec974d53bf45f
  [onnx/model_q8f16.onnx]=04c658aec1b6008857c2ad10f8c589d4180d0ec427e7e6118ceb487e215c3cd0
  [onnx/model_quantized.onnx]=fbae9257e1e05ffc727e951ef9b9c98418e6d79f1c9b6b13bd59f5c9028a1478
  [onnx/model_uint8.onnx]=6607a397d77b8514065420b7c1e7320117f7aabfdb45ce15f0050c5b0fe75aea
  [onnx/model_uint8f16.onnx]=883333e03c597584b532eebea0f8310f25f0c9ade58fe864792c12d969944a9a
  [voices/af.bin]=a4f11d9d055a12bfa0db2668a3e4f0ef8fd1f1ccca69494479718e44dbf9e41a
  [voices/af_alloy.bin]=c4a6b876047fd7fb472edf4ebd63cfac7c3b958a7cae7c106e8f038ca6308c45
  [voices/af_aoede.bin]=4a004c33430762e2461eedb2013fad808ef4ab3121f5300f554476caf58d8361
  [voices/af_bella.bin]=f69d836209b78eb8c66e75e3cda491e26ea838a3674257e9d4e5703cbaf55c8b
  [voices/af_heart.bin]=d583ccff3cdca2f7fae535cb998ac07e9fcb90f09737b9a41fa2734ec44a8f0b
  [voices/af_jessica.bin]=a240a5e3c15b43563d6e923bdca8ef5613a23471d9b77653694012435df23bd8
  [voices/af_kore.bin]=9be5221b6a941c04b561959b8ff0b06e809444dcc4ab7e75a7b23606f691819e
  [voices/af_nicole.bin]=cd2191ab31b914ed7b318416b0e4440fdf392ddad9106a060819aa600a64f59a
  [voices/af_nova.bin]=18778272caa0d0eebaea251c35fd635f038434f9eee5e691d02a174bd328414f
  [voices/af_river.bin]=00a2bcf82b1d86e8f19902ede58c65ccf6c0e43b44b7d74fad54e5d8933c9c30
  [voices/af_sarah.bin]=4409fbc125afabacc615d94db5398d847006a737b0247d6892b7a9a0007a2f0a
  [voices/af_sky.bin]=4435255c9744f3f31659e0d714ab7689bf65d9e77ec1cce060f083912614f0b9
  [voices/am_adam.bin]=162b035ed91cfc48b6046982184c645f72edcdd1b82843347f605d7bf7b15716
  [voices/am_echo.bin]=3968b92c3c4cd1c4416dbded36c13eaa388a90d5788d02a13e4d781f5f8cf3c3
  [voices/am_eric.bin]=e8b5be17edd1e3636901ce7598baafe2dc8dd8ff707a0c23bf9e461add7e2832
  [voices/am_fenrir.bin]=c27989f741f7ee34d273a39d8a595cc0837d35f5ced9a29b7cc162614616df43
  [voices/am_liam.bin]=52403be32fd047c6a44517cb0bcd6b134f2a18baa73e70ef41651e0eab921ade
  [voices/am_michael.bin]=1d1f21dd8da39c30705cd4c75d039d265e9bc4a2a93ed09bc9e1b1225eb95ba1
  [voices/am_onyx.bin]=da5d135b424164916d75a68ffb4c2abce3d7d5ccc82dd1ee6cf447ce286145e6
  [voices/am_puck.bin]=fcf73c989033e9233e0b98713eca600c8c74dcc1614b37009d5450ff4a2274a0
  [voices/am_santa.bin]=61150cf726ab6c5ed7a99f90a304f91f5a72c00c592e89ec94e5df11c319227a
  [voices/bf_alice.bin]=08afa6ba24da61ea5e8efa139e5aadc938d83f0a6da5a900adaf763ac1da5573
  [voices/bf_emma.bin]=669fe0647f9dd04fcab92f1439a40eeb4c8b4ab1f82e4996fe3d918ce4a63b73
  [voices/bf_isabella.bin]=3754352c4aaa46d17f27654ab7518d65b62ad6163a0f55a5f4330c2da2c4e94f
  [voices/bf_lily.bin]=5e0ee32ebe64a467124976b14e69590746f1c4ce41a12b587a50c862edfea335
  [voices/bm_daniel.bin]=6b3194bbceffb746733cbc22c8f593dd44e401a71d53895a2dca891bc595a1e8
  [voices/bm_fable.bin]=f889083196807b4adb15e9204252165f503b8d33d3982e681c52443c49d798f1
  [voices/bm_george.bin]=c4b235a4c1f2cd3b939fed08b899ce9385638b763f7b73a59616c4fc9bd6c9bc
  [voices/bm_lewis.bin]=b8f671cef828c30e66fdf0b0756a76bba58f6bb3398cbbf27058642acbcedb97
  [voices/ef_dora.bin]=f66ec66bd295acb18372e37008533a9a3228483ccd294e7538d5d9294ac9a532
  [voices/em_alex.bin]=27809e9eafdcbcfff90a3016c697568676531de2a2c39cee29c96c7bd6b83e95
  [voices/em_santa.bin]=ad43b774e1ca24d05c6161297d8aeb770ac3d29bb95daf516727af5f7d543683
  [voices/ff_siwis.bin]=a35f5675ad08948e326ae75fd0ea16ba5d0042e4f76b5f3d1df77d0a48c54861
  [voices/hf_alpha.bin]=040be6a4425411cc01fda5fd06693c76bfa78572632852bc8cda9c99232ffb56
  [voices/hf_beta.bin]=cd83ae0bb9b2e4e4fb92b4973bd8d1822ca0036d3c498bf4fc89aa8e33917cc7
  [voices/hm_omega.bin]=b02d9222d9ed00ce26b302173a862c2c93f96cc40b5c422b8d14910b9ff34137
  [voices/hm_psi.bin]=644daf88ba8aeb7bd08950bbdcd4453bb280864e49dc4df93fabc6be32e03f37
  [voices/if_sara.bin]=409b69248798fcdc2542330c76953d230710f19b057e59cb82fdc3c4cf71265c
  [voices/im_nicola.bin]=bc578e510d52a96d6940d46f12e96d7b3df00905dbea075113226d100e6e1ab0
  [voices/jf_alpha.bin]=56b479360aad9f367aeb8cef908f9201cf48b4555e488c5f4590c9dfcd978bb6
  [voices/jf_gongitsune.bin]=0f1181f3772d27b7c12aaf4bcd71e31b186c4146e330d074a3dc64ee392af396
  [voices/jf_nezumi.bin]=13cb71eebb0b48739d444558322aa35a8c9a489b80e1e631f14d2e6aea93026b
  [voices/jf_tebukuro.bin]=29c6c0561b4288d59639677bebe7533c919743d5ea68d0d2ae992644beea6696
  [voices/jm_kumo.bin]=09e959d239724c734d65661f06f14cdabcddfd476bfaaad905a937099ae9e64f
  [voices/pf_dora.bin]=3da7b5b2d91847ebf5646f57631af6ececae3c29a89cd300f06edf9aa6cfe9ee
  [voices/pm_alex.bin]=0175c753f59c54e7fd5a995bedef0c5ff2fb67e0043dd3dcb2ae74ec2acbeb2a
  [voices/pm_santa.bin]=8b012db3185778afe2e45a62cbad69db73021774fe68dda634bcc748a982eede
  [voices/zf_xiaobei.bin]=5dde6e1c9c4f12c8b327bc29c0cee361a23b52b952c04636858ba637ec66e640
  [voices/zf_xiaoni.bin]=08892b62a39af0a615cd0581238db7e19e44c578e8fa0bfd0e586e93327d9cba
  [voices/zf_xiaoxiao.bin]=03adb5d5e3ddd88b047954e974e651cb0a4b524c985057e5d872e962c7be1169
  [voices/zf_xiaoyi.bin]=bc1555c5c486099196ac254bae5e0bb543c121952a3092f50b7d8724f1bc36b3
  [voices/zm_yunjian.bin]=de48a00bdbf3649f07162269a2b6e0513604389bfac8a2e6c75cb34b323ad6fa
  [voices/zm_yunxi.bin]=7243892fb4e560d47014090ddf010f8b8b790f3c6b029ff82b2ac06aa4e27c8b
  [voices/zm_yunxia.bin]=6b2b8fc15b3df19a368daebe5c581c7fabf433ee5b8a17ffd6b3d723cff8936d
  [voices/zm_yunyang.bin]=261e2c89470534dbbcb8fd98b8fdc495ec94063d9bb6c8277f7be43cccba3f42
)

DIR="${XDG_DATA_HOME:-$HOME/.local/share}/stream-engine/models/kokoro"
MODEL="$MODEL_DEFAULT"
ALL_MODELS=0

usage() { sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; }

while (($#)); do
  case "$1" in
    --dir) [[ $# -ge 2 ]] || { echo "error: --dir needs a value" >&2; exit 2; }; DIR="$2"; shift 2 ;;
    --dir=*) DIR="${1#*=}"; shift ;;
    --model) [[ $# -ge 2 ]] || { echo "error: --model needs a value" >&2; exit 2; }; MODEL="$2"; shift 2 ;;
    --model=*) MODEL="${1#*=}"; shift ;;
    --all-models) ALL_MODELS=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "error: unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

MODEL="${MODEL#onnx/}"
if [[ -z "${SUMS[onnx/$MODEL]:-}" ]]; then
  echo "error: unknown model variant '$MODEL'; pinned variants:" >&2
  for k in "${!SUMS[@]}"; do [[ $k == onnx/* ]] && echo "  ${k#onnx/}" >&2; done
  exit 2
fi

for tool in curl sha256sum; do
  command -v "$tool" >/dev/null || { echo "error: $tool not found" >&2; exit 1; }
done

mkdir -p "$DIR/voices"

sum_of() { sha256sum "$1" | cut -d' ' -f1; }

# fetch <repo path> <destination>
fetch() {
  local rel="$1" dest="$2" want="${SUMS[$1]}"
  if [[ -f "$dest" && "$(sum_of "$dest")" == "$want" ]]; then
    echo "ok $rel"
    return 0
  fi
  echo "fetch $rel"
  local part="$dest.part"
  if ! curl -fL --retry 3 --retry-delay 2 --connect-timeout 20 -sS -o "$part" "$BASE/$rel"; then
    rm -f "$part"
    echo "error: download failed: $BASE/$rel" >&2
    return 1
  fi
  local got
  got="$(sum_of "$part")"
  if [[ "$got" != "$want" ]]; then
    rm -f "$part"
    echo "error: checksum mismatch for $rel: expected $want, got $got" >&2
    return 1
  fi
  mv -f "$part" "$dest"
  echo "verified $rel"
}

files=()
if ((ALL_MODELS)); then
  for k in "${!SUMS[@]}"; do [[ $k == onnx/* ]] && files+=("$k"); done
else
  files+=("onnx/$MODEL")
fi
for k in "${!SUMS[@]}"; do [[ $k == voices/* ]] && files+=("$k"); done
mapfile -t files < <(printf '%s\n' "${files[@]}" | sort)

failed=0
n=0
for rel in "${files[@]}"; do
  n=$((n + 1))
  printf '[%d/%d] ' "$n" "${#files[@]}"
  case "$rel" in
    onnx/*) fetch "$rel" "$DIR/${rel#onnx/}" || failed=1 ;;
    voices/*) fetch "$rel" "$DIR/$rel" || failed=1 ;;
  esac
done

cat >"$DIR/SOURCE" <<EOF
Kokoro-82M v1.0 (Apache-2.0) — https://huggingface.co/hexgrad/Kokoro-82M
ONNX export: https://huggingface.co/${REPO} @ ${REV}
Fetched by stream-engine scripts/fetch-tts-model.sh
EOF

if ((failed)); then
  echo "error: some files failed to download or verify (see above)" >&2
  exit 1
fi
echo "done: $DIR (model $MODEL, $(ls "$DIR/voices" | grep -c '\.bin$') voices)"
