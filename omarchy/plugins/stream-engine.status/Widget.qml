import QtQuick
import Quickshell.Io
import qs.Commons
import qs.Ui
import "Model.js" as Model

// Omarchy bar widget for stream-engine: on-air tally + show mode, engine uptime, preflight health,
// and pending approvals. Left click opens (or focuses) the UI, middle click the confidence
// (program) window, right click the details popup.
//
// Data comes from the `streamctl` CLI: one long-running `streamctl --json watch` streams state changes;
// while it is not running (engine stopped or restarting) the widget polls
// `streamctl --json query engine.info` every few seconds and restarts the watch once the engine
// answers. `streamctl --json preflight` refreshes the full checklist periodically.
Panel {
  id: root
  moduleName: "stream-engine.status"

  readonly property string streamCommand: String(setting("streamCommand", "streamctl"))
  readonly property string socketPath: String(setting("socket", ""))
  readonly property string openCommand: String(setting("openCommand", "stream-engine-launch-or-focus"))
  readonly property int preflightIntervalSec: Math.max(5, Number(setting("preflightIntervalSec", 30)) || 30)
  readonly property int retryIntervalSec: Math.max(1, Number(setting("retryIntervalSec", 5)) || 5)
  readonly property bool hideWhenOffline: setting("hideWhenOffline", false) === true

  property bool online: false
  property var info: null
  property var model: Model.emptyState()
  property real now: Date.now() / 1000
  property var themeColors: ({})

  readonly property var health: Model.healthSummary(model.health)
  readonly property int pending: Model.pendingTotal(model)
  readonly property bool live: online && Model.isLive(model)
  readonly property real uptime: online && info && info.started ? Math.max(0, now - Number(info.started)) : 0

  readonly property color fg: bar ? bar.barForeground : Color.foreground
  readonly property color red: themeColors.red || Color.urgent
  readonly property color yellow: themeColors.yellow || themeColors.bright_yellow || Color.accent
  readonly property color green: themeColors.green || fg

  readonly property string glyphOnAir: String.fromCodePoint(0xF0003)   // md-access_point
  readonly property string glyphAlert: String.fromCodePoint(0xF0026)   // md-alert
  readonly property string glyphFail: String.fromCodePoint(0xF0159)    // md-close_circle
  readonly property string glyphPass: String.fromCodePoint(0xF05E0)    // md-check_circle
  readonly property string glyphPending: String.fromCodePoint(0xF01C)  // fa-inbox

  readonly property string barLabel: {
    if (!online) return "ENGINE DOWN"
    var label = Model.modeLabel(model.mode)
    return uptime > 0 ? label + " " + Model.formatDuration(uptime) : label
  }

  readonly property string tooltipText: {
    if (!online) return "stream-engine: engine down\nleft: open UI · right: details"
    var lines = ["stream-engine: " + Model.modeLabel(model.mode) + (live ? " (on air)" : "")]
    if (uptime > 0) lines.push("engine up " + Model.formatDuration(uptime))
    lines.push("preflight: " + health.pass + " pass · " + health.warn + " warn · " + health.fail + " fail")
    if (pending > 0) lines.push(pending + " pending approval" + (pending === 1 ? "" : "s"))
    lines.push("left: open UI · middle: program · right: details")
    return lines.join("\n")
  }

  function cmd(args) {
    var command = [streamCommand, "--json"]
    if (socketPath) command.push("--socket", socketPath)
    return command.concat(args)
  }

  function watchArgs() {
    var args = ["watch", "--events="]
    for (var i = 0; i < Model.WATCH_STATE.length; i++) args.push("--state", Model.WATCH_STATE[i])
    return args
  }

  function setOffline() {
    online = false
    info = null
    model = Model.emptyState()
  }

  function poll() {
    if (!infoProc.running && !watchProc.running) infoProc.running = true
  }

  function refreshPreflight() {
    if (online && !preflightProc.running) preflightProc.running = true
  }

  // Settings changed: drop the running processes; the poll timer reconnects with the new command.
  function restart() {
    watchProc.running = false
    infoProc.running = false
    setOffline()
  }

  function openUi() {
    if (bar) bar.run(openCommand)
  }

  function openProgram() {
    if (bar) bar.run(openCommand + " --program")
  }

  onStreamCommandChanged: restart()
  onSocketPathChanged: restart()

  visible: online || !hideWhenOffline
  implicitWidth: visible ? button.implicitWidth : 0
  implicitHeight: visible ? button.implicitHeight : 0

  Process {
    id: infoProc
    command: root.cmd(["query", "engine.info"])
    stdout: StdioCollector {
      waitForEnd: true
      onStreamFinished: {
        var parsed = Model.parseInfo(text)
        if (!parsed) {
          root.setOffline()
          return
        }
        root.info = parsed
        root.online = true
        if (!watchProc.running) watchProc.running = true
        root.refreshPreflight()
      }
    }
  }

  Process {
    id: watchProc
    command: root.cmd(root.watchArgs())
    stdout: SplitParser {
      onRead: function(line) {
        var next = Model.applyWatchLine(root.model, line)
        if (next) root.model = next
      }
    }
    onExited: root.setOffline()
  }

  Process {
    id: preflightProc
    command: root.cmd(["preflight"])
    stdout: StdioCollector {
      waitForEnd: true
      onStreamFinished: {
        var next = root.online ? Model.applyPreflight(root.model, text) : null
        if (next) root.model = next
      }
    }
  }

  // Fallback polling while the watch is down.
  Timer {
    interval: root.retryIntervalSec * 1000
    running: !watchProc.running
    repeat: true
    triggeredOnStart: true
    onTriggered: root.poll()
  }

  Timer {
    interval: root.preflightIntervalSec * 1000
    running: root.online
    repeat: true
    onTriggered: root.refreshPreflight()
  }

  Timer {
    interval: 1000
    running: root.online
    repeat: true
    onTriggered: root.now = Date.now() / 1000
  }

  // Tally/health colors from the active Omarchy theme (red/yellow/green are not part of qs.Commons).
  FileView {
    id: themeFile
    path: Color.currentThemePath + "/colors.toml"
    watchChanges: true
    onFileChanged: reload()
    onLoaded: root.themeColors = Model.parseColors(text())
  }

  Connections {
    target: Color
    function onUrgentChanged() { themeFile.reload() }
    function onForegroundChanged() { themeFile.reload() }
  }

  onOpenedChanged: if (opened) refreshPreflight()

  WidgetButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    labelVisible: false
    hasVisualContent: true
    dimmed: !root.online
    fixedWidth: vertical ? -1 : content.implicitWidth + scaledHorizontalMargin * 2
    fixedHeight: vertical ? content.implicitHeight + scaledVerticalPadding * 2 : -1
    tooltipText: root.opened ? "" : root.tooltipText
    onPressed: function(b) {
      if (b === Qt.RightButton) root.toggle()
      else if (b === Qt.MiddleButton) root.openProgram()
      else root.openUi()
    }

    Row {
      id: content
      anchors.centerIn: parent
      spacing: Style.space(5)

      BarText {
        text: root.glyphOnAir
        color: root.live ? root.red : root.fg
      }
      BarText {
        visible: !button.vertical
        text: root.barLabel
        color: root.live ? root.red : root.fg
        font.bold: root.live
      }
      BarText {
        visible: !button.vertical && root.online && root.health.fail + root.health.warn > 0
        text: root.glyphAlert + " " + (root.health.fail > 0 ? root.health.fail : root.health.warn)
        color: root.health.fail > 0 ? root.red : root.yellow
      }
      BarText {
        visible: !button.vertical && root.online && root.pending > 0
        text: root.glyphPending + " " + root.pending
      }
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: button
    owner: root
    bar: root.bar
    open: root.opened
    focusTarget: keyCatcher
    contentWidth: panel.fittedContentWidth(Style.space(400))
    contentHeight: panel.fittedContentHeight(column.implicitHeight)

    PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent
      onCloseRequested: root.close()
      onActivateRequested: { root.openUi(); root.close() }
      onTabRequested: function(direction) { root.switchPanel(direction) }
      onTextKey: function(t) {
        if (t === "r") root.refreshPreflight()
        else if (t === "o") { root.openUi(); root.close() }
        else if (t === "p") { root.openProgram(); root.close() }
      }

      Column {
        id: column
        anchors.left: parent.left
        anchors.right: parent.right
        anchors.top: parent.top
        spacing: Style.space(12)

        // ---------- Hero: tally · title/status · uptime ----------
        Item {
          width: parent.width
          implicitHeight: Math.max(heroGlyph.implicitHeight, heroLabels.implicitHeight, heroUptime.implicitHeight)

          PanelText {
            id: heroGlyph
            text: root.glyphOnAir
            color: root.live ? root.red : root.fg
            font.pixelSize: Style.font.display
            anchors.left: parent.left
            anchors.verticalCenter: parent.verticalCenter
          }

          Column {
            id: heroLabels
            anchors.left: heroGlyph.right
            anchors.leftMargin: Style.space(14)
            anchors.right: heroUptime.left
            anchors.rightMargin: Style.space(10)
            anchors.verticalCenter: parent.verticalCenter
            spacing: Style.space(2)

            PanelText {
              text: "stream-engine"
              font.pixelSize: Style.font.title
              font.bold: true
              elide: Text.ElideRight
              width: parent.width
            }
            PanelText {
              text: !root.online ? "ENGINE DOWN"
                : (root.model.panic ? "PANIC · " : "") + Model.modeLabel(root.model.mode) + (root.live ? " · ON AIR" : "")
              color: root.live || root.model.panic ? root.red : Qt.darker(root.fg, 1.4)
              font.pixelSize: Style.font.caption
              font.bold: true
              font.letterSpacing: 1.2
              elide: Text.ElideRight
              width: parent.width
            }
          }

          PanelText {
            id: heroUptime
            text: root.uptime > 0 ? Model.formatDuration(root.uptime) : ""
            font.pixelSize: Style.font.displayLarge
            font.bold: true
            anchors.right: parent.right
            anchors.verticalCenter: parent.verticalCenter
          }
        }

        PanelText {
          visible: !root.online
          width: parent.width
          wrapMode: Text.Wrap
          opacity: 0.7
          font.pixelSize: Style.font.bodySmall
          text: "No engine on " + (root.socketPath || "the default socket")
            + ". Start it with: systemctl --user start stream-engine"
        }

        // ---------- Show ----------
        Column {
          visible: root.online
          width: parent.width
          spacing: Style.spacing.labelGap

          PanelSeparator { foreground: root.fg }
          PanelSectionHeader { text: "SHOW"; foreground: root.fg; fontFamily: root.bar ? root.bar.fontFamily : Style.font.family }
          InfoPair { label: "Program"; value: root.model.program || "—" }
          InfoPair { label: "Preview"; value: root.model.preview || "—" }
          InfoPair {
            label: "OBS"
            value: !root.model.obsLink ? "not connected"
              : (root.model.streaming ? "streaming" : "not streaming") + (root.model.recording ? " · recording" : "")
            valueColor: root.model.streaming ? root.red : (root.model.obsLink ? root.fg : root.yellow)
          }
          InfoPair {
            label: "Engine"
            value: root.info ? String(root.info.version || "") + " · pid " + String(root.info.pid || "") : ""
          }
        }

        // ---------- Preflight ----------
        Column {
          visible: root.online
          width: parent.width
          spacing: Style.spacing.labelGap

          PanelSeparator { foreground: root.fg }
          PanelSectionHeader {
            text: "PREFLIGHT  " + root.health.pass + " PASS · " + root.health.warn + " WARN · " + root.health.fail + " FAIL"
            foreground: root.fg
            fontFamily: root.bar ? root.bar.fontFamily : Style.font.family
          }
          PanelText {
            visible: root.health.problems.length === 0
            text: root.glyphPass + "  all checks pass"
            color: root.green
            font.pixelSize: Style.font.bodySmall
          }
          Repeater {
            model: root.health.problems.slice(0, 8)
            InfoPair {
              required property var modelData
              glyph: modelData.status === "fail" ? root.glyphFail : root.glyphAlert
              glyphColor: modelData.status === "fail" ? root.red : root.yellow
              label: modelData.name
              value: modelData.detail
            }
          }
          PanelText {
            visible: root.health.problems.length > 8
            text: "+" + (root.health.problems.length - 8) + " more in the UI's preflight panel"
            opacity: 0.6
            font.pixelSize: Style.font.caption
          }
        }

        // ---------- Pending approvals ----------
        Column {
          visible: root.online
          width: parent.width
          spacing: Style.spacing.labelGap

          PanelSeparator { foreground: root.fg }
          PanelSectionHeader { text: "PENDING APPROVALS"; foreground: root.fg; fontFamily: root.bar ? root.bar.fontFamily : Style.font.family }
          InfoPair { label: "Song requests"; value: String(root.model.queuePending); valueColor: root.model.queuePending > 0 ? root.yellow : root.fg }
          InfoPair { label: "Alerts in veto window"; value: String(root.model.alertsVeto); valueColor: root.model.alertsVeto > 0 ? root.yellow : root.fg }
          InfoPair { label: "AutoMod held"; value: String(root.model.automodHeld); valueColor: root.model.automodHeld > 0 ? root.yellow : root.fg }
          InfoPair { label: "Policy approvals"; value: String(root.model.policyPending); valueColor: root.model.policyPending > 0 ? root.yellow : root.fg }
        }

        // ---------- Actions ----------
        Row {
          width: parent.width
          spacing: Style.space(6)
          readonly property real cellWidth: (width - spacing * 2) / 3

          Button {
            width: parent.cellWidth
            text: "Open UI"
            fontSize: Style.font.bodySmall
            foreground: root.fg
            fontFamily: root.bar ? root.bar.fontFamily : Style.font.family
            bordered: true
            onClicked: { root.openUi(); root.close() }
          }
          Button {
            width: parent.cellWidth
            text: "Program"
            fontSize: Style.font.bodySmall
            foreground: root.fg
            fontFamily: root.bar ? root.bar.fontFamily : Style.font.family
            bordered: true
            onClicked: { root.openProgram(); root.close() }
          }
          Button {
            width: parent.cellWidth
            text: "Refresh"
            fontSize: Style.font.bodySmall
            foreground: root.fg
            fontFamily: root.bar ? root.bar.fontFamily : Style.font.family
            bordered: true
            onClicked: root.online ? root.refreshPreflight() : root.poll()
          }
        }
      }
    }
  }

  component BarText: Text {
    textFormat: Text.PlainText
    color: root.fg
    font.family: root.bar ? root.bar.fontFamily : Style.font.family
    font.pixelSize: Style.font.body
    renderType: Text.NativeRendering
    anchors.verticalCenter: parent ? parent.verticalCenter : undefined
  }

  component PanelText: Text {
    textFormat: Text.PlainText
    color: root.fg
    font.family: root.bar ? root.bar.fontFamily : Style.font.family
    font.pixelSize: Style.font.body
  }

  component InfoPair: Item {
    property string glyph: ""
    property color glyphColor: root.fg
    property string label: ""
    property string value: ""
    property color valueColor: root.fg

    width: parent ? parent.width : 0
    implicitHeight: Math.max(labelText.implicitHeight, valueText.implicitHeight)

    Text {
      id: glyphText
      visible: parent.glyph !== ""
      textFormat: Text.PlainText
      text: parent.glyph
      color: parent.glyphColor
      font.family: root.bar ? root.bar.fontFamily : Style.font.family
      font.pixelSize: Style.font.bodySmall
      anchors.left: parent.left
      anchors.verticalCenter: parent.verticalCenter
    }
    Text {
      id: labelText
      textFormat: Text.PlainText
      text: parent.label
      color: root.fg
      opacity: 0.6
      font.family: root.bar ? root.bar.fontFamily : Style.font.family
      font.pixelSize: Style.font.bodySmall
      anchors.left: glyphText.visible ? glyphText.right : parent.left
      anchors.leftMargin: glyphText.visible ? Style.space(6) : 0
      anchors.verticalCenter: parent.verticalCenter
    }
    Text {
      id: valueText
      textFormat: Text.PlainText
      text: parent.value
      color: parent.valueColor
      elide: Text.ElideLeft
      horizontalAlignment: Text.AlignRight
      font.family: root.bar ? root.bar.fontFamily : Style.font.family
      font.pixelSize: Style.font.bodySmall
      anchors.left: labelText.right
      anchors.leftMargin: Style.space(12)
      anchors.right: parent.right
      anchors.verticalCenter: parent.verticalCenter
    }
  }
}
