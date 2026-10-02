#!/usr/bin/env sh
set -eu

DESTINATION="${CCM_INSTALL_DIR:-$HOME/.local/bin}"

cargo build --release

mkdir -p "$DESTINATION"
install -m 0755 target/release/ccm "$DESTINATION/ccm"

case ":$PATH:" in
  *":$DESTINATION:"*) ;;
  *)
    echo "note: $DESTINATION is not on your PATH; add it to your shell profile"
    ;;
esac

"$DESTINATION/ccm" --version
echo "Installed: $DESTINATION/ccm"
