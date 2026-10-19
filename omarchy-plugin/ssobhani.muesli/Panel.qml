import QtQuick
import QtQuick.Controls
import QtQuick.Shapes
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui

// Muesli in the bar: one long-lived `muesli-cli watch --json` stream is the
// only source of truth (no polling); every action is a short-lived
// `muesli-cli <cmd>` process. The daemon sends a full Status snapshot on every
// change, so this widget keeps no state of its own beyond optimistic UI.
Panel {
  id: root
  moduleName: "ssobhani.muesli"
  ipcTarget: "ssobhani.muesli"
  manageIpc: false

  // ------------------------------------------------------------------ theme

  readonly property color foreground: bar ? bar.foreground : Color.foreground
  readonly property color barFg: bar ? bar.barForeground : Color.foreground
  readonly property color accent: Color.accent
  readonly property color red: bar ? bar.urgent : Color.bar.active
  readonly property color muted: Color.muted
  readonly property color onAccent: Color.background
  readonly property color hairline: Util.alpha(foreground, 0.12)
  readonly property string fontFamily: bar ? bar.fontFamily : Style.font.family

  // --------------------------------------------------------------- settings

  function expandHome(p) { return p.indexOf("~/") === 0 ? Quickshell.env("HOME") + p.slice(1) : p }
  readonly property string cliPath: expandHome(String(setting("cliPath", "muesli-cli")))
  readonly property bool showTimer: setting("showTimer", true) !== false

  // ---------------------------------------------------------- daemon state

  property bool connected: false
  property string offlineReason: ""
  property var status: null
  property var autoRecordPending: null
  property string actionError: ""
  property double nowMs: Date.now()

  readonly property int meterBars: 24
  property var meHistory: zeros(meterBars)
  property var themHistory: zeros(meterBars)

  readonly property var session: status && status.session ? status.session : ({ kind: "idle" })
  readonly property var processing: status ? (status.processing || []) : []
  readonly property bool autoRecord: autoRecordPending !== null ? autoRecordPending : !!(status && status.auto_record)

  // Bar priority: what needs you now > what's happening.
  readonly property string mode: !connected ? "offline"
    : session.kind === "recording" ? "recording"
    : session.kind === "prompting" ? "prompting"
    : processing.length > 0 ? "processing"
    : "idle"
  // The panel shows processing as a progress line in the idle layout.
  readonly property string panelMode: mode === "processing" ? "idle" : mode

  // ------------------------------------------------------------ pure helpers

  function zeros(n) { return Array.from({ length: n }, () => 0) }
  function appLabel(key) {
    var labels = { zoom: "Zoom", discord: "Discord" }
    return !key ? "" : (labels[key] || (String(key).charAt(0).toUpperCase() + String(key).slice(1)))
  }
  function pad2(n) { return (n < 10 ? "0" : "") + n }
  function clock(ms) {
    var s = Math.max(0, Math.floor(ms / 1000))
    return s >= 3600 ? Math.floor(s / 3600) + ":" + pad2(Math.floor(s / 60) % 60) + ":" + pad2(s % 60)
                     : pad2(Math.floor(s / 60)) + ":" + pad2(s % 60)
  }
  function stepsDone(p) { return p && p.pipeline ? p.pipeline.steps.filter(s => s.status === "done").length : 0 }
  function stepsTotal(p) { return p && p.pipeline ? p.pipeline.steps.length : 4 }
  function activeStep(p) {
    var labels = { saved: "Saving recording…", cleaned: "Cleaning audio…", transcribed: "Transcribing…", notes: "Notes in progress…" }
    var failed = p && p.pipeline ? p.pipeline.steps.find(s => s.status === "failed") : null
    if (failed) return "Failed — reprocess from the app"
    var active = p && p.pipeline ? p.pipeline.steps.find(s => s.status === "active") : null
    return active ? (labels[active.kind] || "Working…") : "Queued"
  }
  function unique(xs) { return xs.filter((x, i) => x && xs.indexOf(x) === i) }
  readonly property double recordingMs: session.kind === "recording" ? nowMs - Number(session.started_at || nowMs) : 0
  readonly property string sessionLine: unique([appLabel(session.app), session.hint || ""]).join(" · ")

  function tooltip() {
    switch (mode) {
    case "offline": return "muesli-daemon not running"
    case "prompting": return "Call detected in " + appLabel(session.app) + " — click to record"
    case "recording": return "Recording " + (session.app ? appLabel(session.app) : "manually") + " · " + clock(recordingMs) + " — right-click to stop"
    case "processing": return "Wrapping up " + stepsDone(processing[0]) + "/" + stepsTotal(processing[0]) + " · " + activeStep(processing[0])
    default: return "Muesli · listening for calls" + (autoRecord ? " · auto-record on" : "")
    }
  }

  // ------------------------------------------------------------ event stream

  function handleLine(line) {
    var msg
    try { msg = JSON.parse(line) } catch (e) { return }
    if (!msg || typeof msg !== "object") return
    if (msg.event === "status") {
      connected = true
      offlineReason = ""
      status = msg.data
      if (autoRecordPending !== null && !!msg.data.auto_record === autoRecordPending) autoRecordPending = null
    } else if (msg.event === "levels") {
      if (root.opened && root.panelMode === "recording") pushLevels(msg.data || {})
    } else if (msg.event === "disconnected") {
      connected = false
      offlineReason = msg.data && msg.data.error ? String(msg.data.error) : ""
    }
  }

  function pushLevels(l) {
    var clamp01 = v => Math.max(0, Math.min(1, Number(v) || 0))
    meHistory = meHistory.slice(1).concat([clamp01(l.me)])
    themHistory = themHistory.slice(1).concat([clamp01(l.them)])
  }

  Process {
    id: watcher
    command: [root.cliPath, "watch", "--json"]
    running: true
    stdout: SplitParser { onRead: data => root.handleLine(data) }
    onRunningChanged: if (!running) {
      root.connected = false
      if (root.offlineReason === "") root.offlineReason = "muesli-cli watch exited"
      restartWatcher.restart()
    }
  }

  // `watch` retries on its own; this only covers the CLI itself dying or
  // being missing from PATH.
  Timer { id: restartWatcher; interval: 5000; onTriggered: watcher.running = true }
  // Settings land after construction; only bounce a stream that is live.
  onCliPathChanged: if (watcher.running) watcher.running = false

  // ------------------------------------------------------------------ actions

  Component {
    id: actionProcess
    Process {
      stderr: StdioCollector {
        onStreamFinished: {
          var t = text.trim().replace(/^Error:\s*/, "")
          if (t !== "") { root.actionError = t; clearError.restart() }
        }
      }
      onExited: function(code) {
        if (code !== 0) root.autoRecordPending = null
        destroy(1000)
      }
    }
  }

  function run(args) {
    actionError = ""
    actionProcess.createObject(root, { command: [cliPath].concat(args), running: true })
  }
  function setAutoRecord(v) { autoRecordPending = v; run(["auto-record", v ? "on" : "off"]) }
  function openNotes(id) { run(id ? ["open", id] : ["open"]) }

  Timer { id: clearError; interval: 8000; onTriggered: root.actionError = "" }

  // Ticks only when a visible clock needs it.
  Timer {
    interval: 1000
    repeat: true
    triggeredOnStart: true
    running: (root.mode === "recording" && (root.showTimer || root.opened)) || (root.mode === "prompting" && root.opened)
    onTriggered: root.nowMs = Date.now()
  }

  property real pulse: 1
  SequentialAnimation on pulse {
    running: root.mode === "prompting"
    loops: Animation.Infinite
    NumberAnimation { to: 0.45; duration: 900; easing.type: Easing.InOutSine }
    NumberAnimation { to: 1; duration: 900; easing.type: Easing.InOutSine }
    onRunningChanged: if (!running) root.pulse = 1
  }

  onOpenedChanged: if (opened) {
    meHistory = zeros(meterBars)
    themHistory = zeros(meterBars)
    nowMs = Date.now()
  }

  IpcHandler {
    target: root.ipcTarget
    function open(): void { root.open() }
    function close(): void { root.close() }
    function toggle(): void { root.toggle() }
  }

  // ------------------------------------------------------------- components

  component Label: Text {
    textFormat: Text.PlainText
    color: root.foreground
    font.family: root.fontFamily
    font.pixelSize: Style.font.body
    elide: Text.ElideRight
    verticalAlignment: Text.AlignVCenter
  }

  component Caps: Label {
    font.pixelSize: Style.font.bodySmall
    font.letterSpacing: 0.8
    font.capitalization: Font.AllUppercase
  }

  component Dot: Rectangle {
    property color tint: root.foreground
    property bool hollow: false
    property real size: Style.space(8)
    width: size
    height: size
    radius: size / 2
    color: hollow ? "transparent" : tint
    border.width: hollow ? Math.max(1, Style.spaceReal(1.5)) : 0
    border.color: tint
    anchors.verticalCenter: parent ? parent.verticalCenter : undefined
  }

  // The muesli mark: a mountain ridge, 18×10.
  component Ridge: Shape {
    id: ridge
    property color stroke: root.foreground
    implicitWidth: 18
    implicitHeight: 10
    preferredRendererType: Shape.CurveRenderer
    anchors.verticalCenter: parent ? parent.verticalCenter : undefined
    ShapePath {
      strokeColor: ridge.stroke
      strokeWidth: 1.3
      fillColor: "transparent"
      capStyle: ShapePath.RoundCap
      joinStyle: ShapePath.RoundJoin
      PathSvg { path: "M1 9 L6 4 L8 6 L12 1 L17 9" }
    }
  }

  component Spinner: Shape {
    id: spinner
    property color track: Util.alpha(root.foreground, 0.18)
    property color arc: root.accent
    implicitWidth: 12
    implicitHeight: 12
    preferredRendererType: Shape.CurveRenderer
    anchors.verticalCenter: parent ? parent.verticalCenter : undefined
    ShapePath {
      strokeColor: spinner.track
      strokeWidth: 1.5
      fillColor: "transparent"
      PathAngleArc { centerX: 6; centerY: 6; radiusX: 4.5; radiusY: 4.5; startAngle: 0; sweepAngle: 360 }
    }
    ShapePath {
      strokeColor: spinner.arc
      strokeWidth: 1.5
      fillColor: "transparent"
      capStyle: ShapePath.RoundCap
      PathAngleArc { centerX: 6; centerY: 6; radiusX: 4.5; radiusY: 4.5; startAngle: -90; sweepAngle: 90 }
    }
    RotationAnimator on rotation {
      from: 0; to: 360; duration: 1100
      loops: Animation.Infinite
      running: spinner.visible
    }
  }

  component ActionButton: Rectangle {
    id: action
    property string text: ""
    property bool primary: false
    property color fill: root.accent
    property bool marker: false
    signal clicked()

    implicitHeight: Style.space(34)
    radius: Style.cornerRadius
    color: primary
      ? (mouse.pressed ? Qt.darker(fill, 1.15) : mouse.containsMouse ? Qt.lighter(fill, 1.08) : fill)
      : (mouse.pressed ? Style.pressedFillFor(root.foreground, root.accent)
         : mouse.containsMouse ? Style.hoverFillFor(root.foreground, root.accent)
         : Util.alpha(root.foreground, 0.02))
    border.width: primary ? 0 : 1
    border.color: mouse.containsMouse ? Util.alpha(root.foreground, 0.3) : root.hairline

    Behavior on color { ColorAnimation { duration: 120 } }

    Row {
      anchors.centerIn: parent
      spacing: Style.space(8)
      Dot { visible: action.marker; size: Style.space(7); tint: action.primary ? root.onAccent : action.fill }
      Label {
        text: action.text
        color: action.primary ? root.onAccent : root.foreground
        font.pixelSize: Style.font.subtitle
        font.bold: action.primary
      }
    }

    MouseArea {
      id: mouse
      anchors.fill: parent
      hoverEnabled: true
      cursorShape: Qt.PointingHandCursor
      onClicked: action.clicked()
    }
  }

  component ButtonPair: Row {
    width: parent ? parent.width : 0
    spacing: Style.space(8)
    readonly property real cell: (width - spacing) / 2
  }

  // -------------------------------------------------------------------- bar

  visible: true
  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  WidgetButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    labelVisible: false
    hasVisualContent: true
    tooltipText: root.tooltip()
    fixedWidth: vertical ? -1 : Math.max(Style.bar.iconSlot, pill.width + Style.space(10))
    fixedHeight: vertical ? Math.max(Style.bar.iconSlot, pill.height + Style.space(8)) : -1
    onPressed: function(code) {
      if (code === Qt.RightButton) root.run(["toggle"])
      else if (code === Qt.MiddleButton) root.openNotes("")
      else root.toggle()
    }

    Rectangle {
      id: pill
      anchors.centerIn: parent
      readonly property bool framed: !button.vertical && (root.mode === "prompting" || root.mode === "recording")
      width: barRow.implicitWidth + (framed ? Style.space(16) : 0)
      height: framed ? Math.min(button.barSize - Style.space(6), Style.space(20)) : barRow.implicitHeight
      radius: Style.cornerRadius
      color: root.mode === "recording" && framed ? Util.alpha(root.barFg, 0.12) : "transparent"
      border.width: root.mode === "prompting" && framed ? 1 : 0
      border.color: root.accent
      opacity: root.mode === "prompting" ? root.pulse : 1

      Row {
        id: barRow
        anchors.centerIn: parent
        spacing: Style.space(6)

        Ridge {
          visible: root.mode === "idle" || root.mode === "offline"
          stroke: root.barFg
          opacity: root.mode === "offline" ? 0.35 : 0.6
        }
        Dot {
          visible: root.mode === "prompting"
          size: Style.space(7)
          hollow: true
          tint: root.accent
        }
        Dot {
          visible: root.mode === "recording"
          size: Style.space(7)
          tint: root.red
        }
        Spinner {
          visible: root.mode === "processing"
          arc: root.accent
          track: Util.alpha(root.barFg, 0.2)
        }
        Label {
          visible: !button.vertical && text !== ""
          height: pill.height
          text: root.mode === "prompting" ? "Record?"
            : root.mode === "recording" ? (root.showTimer ? root.clock(root.recordingMs) : "REC")
            : root.mode === "processing" ? root.stepsDone(root.processing[0]) + "/" + root.stepsTotal(root.processing[0])
            : ""
          color: root.mode === "prompting" ? root.accent : root.mode === "processing" ? root.muted : root.barFg
          font.pixelSize: Style.font.body
          font.features: { "tnum": 1 }
        }
      }
    }
  }

  // ------------------------------------------------------------------ panel

  KeyboardPanel {
    id: panel
    anchorItem: button
    owner: root
    bar: root.bar
    open: root.opened
    focusTarget: keyCatcher
    contentWidth: panel.fittedContentWidth(Style.space(320))
    contentHeight: panel.fittedContentHeight(column.implicitHeight, Style.space(560))

    PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent

      onCloseRequested: root.close()
      onTabRequested: function(direction) { root.switchPanel(direction) }
      onActivateRequested: if (root.panelMode === "prompting") root.run(["record"])
      onTextKey: function(t) {
        if (root.panelMode === "prompting" && (t === "r" || t === "R")) root.run(["record"])
        else if (root.panelMode === "prompting" && (t === "i" || t === "I")) root.run(["ignore"])
        else if (t === "o" || t === "O") root.openNotes("")
        else if (t === "a" || t === "A") root.setAutoRecord(!root.autoRecord)
      }

      Column {
        id: column
        width: parent.width
        spacing: Style.space(16)

        // ---------- status line: dot · LABEL ........ meta ----------
        Item {
          width: parent.width
          height: Style.space(18)

          Row {
            id: statusRow
            anchors.left: parent.left
            anchors.verticalCenter: parent.verticalCenter
            spacing: Style.space(8)

            readonly property color tint: root.panelMode === "prompting" ? root.accent
              : root.panelMode === "recording" ? root.red
              : root.muted

            Spinner { visible: root.panelMode === "idle" && root.processing.length > 0 }
            Dot {
              visible: !(root.panelMode === "idle" && root.processing.length > 0)
              tint: statusRow.tint
              hollow: root.panelMode === "prompting" || root.panelMode === "offline"
              opacity: root.panelMode === "recording" ? root.pulse : 1
            }
            Caps {
              text: root.panelMode === "prompting" ? "Call detected"
                : root.panelMode === "recording" ? "Rec"
                : root.panelMode === "offline" ? "Offline"
                : root.processing.length > 0 ? "Wrapping up"
                : "Listening"
              color: statusRow.tint
            }
          }

          Label {
            anchors.right: parent.right
            anchors.verticalCenter: parent.verticalCenter
            width: Math.min(implicitWidth, parent.width - statusRow.width - Style.space(12))
            horizontalAlignment: Text.AlignRight
            color: root.muted
            text: root.panelMode === "prompting" ? root.appLabel(root.session.app)
              : root.panelMode === "recording" ? (root.session.app ? root.appLabel(root.session.app) : "Manual")
              : root.panelMode === "idle" ? (root.autoRecord ? "auto-record on" : "asks first")
              : ""
          }
        }

        // ---------- prompting: what was detected ----------
        Column {
          visible: root.panelMode === "prompting"
          width: parent.width
          spacing: Style.space(4)

          Label {
            width: parent.width
            text: root.session.hint || (root.appLabel(root.session.app) + " call")
            color: Color.popups.text
            font.pixelSize: Style.space(18)
            font.weight: Font.DemiBold
            font.letterSpacing: -0.2
            wrapMode: Text.Wrap
            maximumLineCount: 2
          }
          Label {
            width: parent.width
            color: root.muted
            text: "Asked " + Math.max(0, Math.round((root.nowMs - Number(root.session.since || root.nowMs)) / 1000)) + " s ago · nothing recorded yet"
          }
        }

        ButtonPair {
          id: promptButtons
          visible: root.panelMode === "prompting"
          ActionButton { width: promptButtons.cell; primary: true; marker: true; text: "Record"; onClicked: root.run(["record"]) }
          ActionButton { width: promptButtons.cell; text: "Ignore this call"; onClicked: root.run(["ignore"]) }
        }

        // ---------- recording: timer + meters ----------
        Column {
          visible: root.panelMode === "recording"
          width: parent.width
          spacing: Style.space(4)

          Label {
            text: root.clock(root.recordingMs)
            color: Color.popups.text
            font.pixelSize: Style.space(40)
            font.weight: Font.Light
            font.letterSpacing: -0.8
            font.features: { "tnum": 1 }
          }
          Label {
            width: parent.width
            visible: text !== ""
            color: root.muted
            text: root.sessionLine || "Manual recording"
          }
        }

        Column {
          visible: root.panelMode === "recording"
          width: parent.width
          spacing: Style.space(10)

          Item {
            id: meters
            width: parent.width
            height: Style.space(28)
            readonly property real gap: Style.space(20)
            readonly property real half: (width - gap) / 2

            Repeater {
              model: root.meterBars * 2
              Rectangle {
                required property int index
                readonly property bool them: index >= root.meterBars
                readonly property int slot: index % root.meterBars
                readonly property real level: them ? root.themHistory[slot] : root.meHistory[slot]
                readonly property real step: meters.half / root.meterBars
                width: Math.max(1.5, Style.spaceReal(2))
                height: Math.max(width, Style.space(2) + level * (meters.height - Style.space(2)))
                radius: width / 2
                x: (them ? meters.half + meters.gap : 0) + slot * step + (step - width) / 2
                anchors.verticalCenter: parent.verticalCenter
                color: them ? root.accent : Color.popups.text
                opacity: level > 0.02 ? 1 : 0.55
                Behavior on height { NumberAnimation { duration: 110 } }
              }
            }
          }

          Item {
            width: parent.width
            height: youLabel.implicitHeight
            Label { id: youLabel; text: "you"; color: root.muted; font.pixelSize: Style.font.bodySmall }
            Label { anchors.right: parent.right; text: "them"; color: root.muted; font.pixelSize: Style.font.bodySmall }
          }
        }

        ButtonPair {
          id: recordButtons
          visible: root.panelMode === "recording"
          ActionButton { width: recordButtons.cell; primary: true; fill: root.red; text: "Stop"; onClicked: root.run(["stop"]) }
          ActionButton { width: recordButtons.cell; text: "Discard"; onClicked: root.run(["discard"]) }
        }

        // Back-to-back meetings: close this one and keep recording into a new one.
        ActionButton {
          visible: root.panelMode === "recording"
          width: parent.width
          text: "Next meeting  →"
          onClicked: root.run(["next"])
        }

        // ---------- idle / offline ----------
        Column {
          visible: root.panelMode === "idle" || root.panelMode === "offline"
          width: parent.width
          spacing: Style.space(4)

          Label {
            width: parent.width
            text: root.panelMode === "offline" ? "muesli-daemon not running" : "Listening for calls"
            color: Color.popups.text
            font.pixelSize: Style.font.heading
            font.weight: Font.DemiBold
          }
          Label {
            width: parent.width
            color: root.muted
            wrapMode: Text.Wrap
            maximumLineCount: 3
            text: root.panelMode === "offline"
              ? "Start muesli-daemon and this updates on its own."
              : root.autoRecord ? "Zoom and Discord calls start recording on their own."
                                : "You'll be asked before a Zoom or Discord call is recorded."
          }
        }

        ButtonPair {
          id: idleButtons
          visible: root.panelMode === "idle"
          ActionButton { width: idleButtons.cell; marker: true; fill: root.red; text: "Record now"; onClicked: root.run(["start"]) }
          ActionButton { width: idleButtons.cell; text: "Open notes"; onClicked: root.openNotes("") }
        }

        // ---------- wrapping up (any state) ----------
        Column {
          visible: root.processing.length > 0 && root.panelMode !== "recording"
          width: parent.width
          spacing: Style.space(6)

          Repeater {
            model: root.processing
            Item {
              required property var modelData
              width: parent.width
              height: Style.space(18)

              Row {
                id: wrapRow
                anchors.left: parent.left
                anchors.verticalCenter: parent.verticalCenter
                spacing: Style.space(8)
                Spinner {}
                Label {
                  text: "Wrapping up " + root.stepsDone(modelData) + "/" + root.stepsTotal(modelData) + " · " + root.activeStep(modelData)
                  font.pixelSize: Style.font.bodySmall
                  font.features: { "tnum": 1 }
                }
              }
              Label {
                anchors.right: parent.right
                anchors.verticalCenter: parent.verticalCenter
                width: Math.max(0, Math.min(implicitWidth, parent.width - wrapRow.width - Style.space(12)))
                horizontalAlignment: Text.AlignRight
                text: modelData.title || ""
                color: root.muted
                font.pixelSize: Style.font.bodySmall
              }
            }
          }
        }

        // ---------- warnings ----------
        Label {
          visible: root.connected && root.status !== null && root.status.api_key === false && root.panelMode !== "recording"
          width: parent.width
          wrapMode: Text.Wrap
          color: root.muted
          font.pixelSize: Style.font.bodySmall
          text: "No OpenRouter key — notes stay transcript-only. Run: muesli-cli set-key"
        }

        Label {
          visible: text !== ""
          width: parent.width
          wrapMode: Text.Wrap
          maximumLineCount: 3
          color: root.red
          font.pixelSize: Style.font.bodySmall
          text: root.actionError !== "" ? root.actionError
            : root.panelMode === "offline" && root.offlineReason.indexOf("not running") === -1 ? root.offlineReason
            : ""
        }

        // ---------- auto-record ----------
        Column {
          visible: root.panelMode === "prompting" || root.panelMode === "recording" || root.panelMode === "idle"
          width: parent.width
          spacing: Style.space(10)

          Rectangle { width: parent.width; height: 1; color: root.hairline }

          Item {
            width: parent.width
            height: autoSwitch.implicitHeight

            Label {
              anchors.left: parent.left
              anchors.verticalCenter: parent.verticalCenter
              text: "Auto-record calls"
              color: Qt.darker(Color.popups.text, 1.1)
            }
            ToggleSwitch {
              id: autoSwitch
              anchors.right: parent.right
              anchors.verticalCenter: parent.verticalCenter
              cursorRing: false
              trackHeight: Style.space(16)
              checked: root.autoRecord
              foreground: root.foreground
              accent: root.accent
              onToggled: root.setAutoRecord(!root.autoRecord)
            }
          }
        }
      }
    }
  }
}
