#!/bin/bash
# Load the plugin in a throwaway Quickshell instance and print QML errors +
# the widget's derived state every 1.5 s. The live omarchy-shell's logs are
# unreliable (`qs log` chokes on the big log file) and `rescanPlugins` keeps
# serving stale compiled QML, so use this for fast iteration and
# `omarchy restart shell` to see changes in the real bar.
#   omarchy-plugin/dev/check.sh [seconds]
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
ln -s /usr/share/omarchy/shell/Commons "$tmp/Commons"
ln -s /usr/share/omarchy/shell/Ui "$tmp/Ui"
cat > "$tmp/shell.qml" <<QML
import QtQuick
import Quickshell

ShellRoot {
  id: shellRoot
  property var w: null
  Component.onCompleted: {
    var c = Qt.createComponent("file://$here/../ssobhani.muesli/Panel.qml")
    if (c.status === Component.Error) { console.warn("LOAD ERROR: " + c.errorString()); Qt.quit(); return }
    w = c.createObject(holder, { settings: { cliPath: "muesli-cli", showTimer: true } })
  }
  Item { id: holder }
  Timer {
    interval: 1500; repeat: true; running: true
    onTriggered: if (shellRoot.w) console.warn("STATE mode=" + shellRoot.w.mode + " panel=" + shellRoot.w.panelMode + " | " + shellRoot.w.tooltip())
  }
}
QML
timeout "${1:-5}" qs -p "$tmp" 2>&1 | grep -E "LOAD ERROR|STATE|ERROR|WARN.*(qml|scene)|TypeError|ReferenceError" | grep -v "portal" || true
