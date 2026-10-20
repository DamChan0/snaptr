# snaptr

화면 일부를 드래그로 지정하면 그 안의 텍스트를 OCR로 읽어서 번역을 원문 위/아래에
덧씌워 보여주는 Linux X11용 1회성 스크린 번역 오버레이. 데몬/트레이 없음 — GNOME
단축키로 실행하는 단발 프로세스.

## 의존성 설치

```sh
sudo apt install tesseract-ocr tesseract-ocr-kor tesseract-ocr-jpn tesseract-ocr-chi-sim xclip
```

(영어만 쓸 거면 `tesseract-ocr`만으로도 충분 — `eng` 데이터는 기본 포함.)

## 빌드 및 설치

```sh
cargo build --release
mkdir -p ~/.local/bin
cp target/release/snaptr ~/.local/bin/snaptr
```

`~/.local/bin`이 `PATH`에 있는지 확인 (`echo $PATH`). GNOME 기본 설정에서는 보통
이미 포함되어 있음.

## CLI

```
snaptr [--full] [--to <lang>] [--ocr <tess langs>]
```

- `--to <lang>`: 번역 대상 언어 코드 (Google Translate 코드, 기본값 `ko`).
- `--ocr <langs>`: tesseract 언어 코드, `+`로 복수 지정 (기본값 `eng`, 예: `eng+jpn+chi_sim`).
- `--full`: 드래그 없이 커서가 있는 모니터 전체를 즉시 캡처.

## GNOME 커스텀 단축키 등록

GNOME은 커스텀 키 바인딩을 `org.gnome.settings-daemon.plugins.media-keys` 스키마의
`custom-keybindings` 리스트로 관리한다. 예시로 드래그 모드와 전체 모니터 모드 두
개를 등록:

```sh
# 1) 리스트에 두 슬롯 추가 (기존 커스텀 단축키가 있으면 배열에 이어붙일 것)
gsettings set org.gnome.settings-daemon.plugins.media-keys custom-keybindings \
  "['/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom0/', \
    '/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom1/']"

# 2) 드래그 모드 (Super+T)
gsettings set org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom0/ \
  name 'snaptr (drag)'
gsettings set org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom0/ \
  command '/home/YOUR_USER/.local/bin/snaptr --to ko --ocr eng+kor+jpn+chi_sim'
gsettings set org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom0/ \
  binding '<Super>t'

# 3) 전체 모니터 모드 (Super+Shift+T)
gsettings set org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom1/ \
  name 'snaptr (full monitor)'
gsettings set org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom1/ \
  command '/home/YOUR_USER/.local/bin/snaptr --full --to ko --ocr eng+kor+jpn+chi_sim'
gsettings set org.gnome.settings-daemon.plugins.media-keys.custom-keybinding:/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/custom1/ \
  binding '<Super><Shift>t'
```

`command` 경로의 `YOUR_USER`를 실제 사용자 이름으로, `~/.local/bin/snaptr`를 설치
경로에 맞게 바꿀 것. GNOME 설정 앱(설정 → 키보드 → 바로가기 → 사용자 지정 바로가기)
에서도 동일하게 등록 가능.

## 사용법

1. 단축키를 누르면 화면이 고정되고 커서가 십자선으로 바뀐다.
2. 드래그 모드 (`snaptr`): 마우스로 영역을 드래그해서 지정. 드래그 없이(5px 미만
   이동) 클릭만 하면 클릭한 지점이 속한 모니터 전체를 선택한 것으로 처리.
3. 전체 모니터 모드 (`--full`): 드래그 없이 커서가 있는 모니터 전체를 바로 캡처.
4. 선택 영역 안에서 텍스트 블록이 감지되면 원문 위에 노란 하이라이트가, 블록
   위나 아래(공간에 따라 자동 선택)에 번역 박스가 나타난다.
5. 번역 박스를 클릭 → 번역문이 클립보드에 복사됨 (테두리가 초록색으로 깜빡임).
6. 하이라이트된 원문 블록을 클릭 → 원문이 클립보드에 복사됨.
7. `Esc` 또는 우클릭 → 종료.

OCR이나 번역 중에는 선택 영역 근처에 "Translating…" 표시가 뜬다. tesseract가
설치되어 있지 않거나 네트워크 오류가 나면 메시지 박스가 뜬 뒤 종료된다.
