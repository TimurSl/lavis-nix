package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func boolPtr(value bool) *bool { return &value }

func testModule(t *testing.T) *Module {
	t.Helper()
	return &Module{
		path:  filepath.Join(t.TempDir(), "state.json"),
		state: defaultState(),
		now:   func() time.Time { return time.Unix(1000, 0) },
	}
}

func timerRequest(id, eventID string) Message {
	return Message{ProtocolVersion: protocolVersion, Type: "event", RequestID: id, Event: "timer.tick", Payload: json.RawMessage(`{"event_id":"` + eventID + `"}`)}
}

func TestTimerWireWaitsForResultBeforeTerminalEventResult(t *testing.T) {
	m := testModule(t)
	responses := m.handleEvent(timerRequest("42", "tick-1"))
	if len(responses) != 1 || responses[0].Type != "telegram.invoke" {
		t.Fatalf("responses: %#v", responses)
	}
	invoke := responses[0]
	if invoke.RequestID != "42" || invoke.CallID != "online_1" || invoke.Method != "account.updateStatus" || string(invoke.Params) != `{"offline":false}` {
		t.Fatalf("invoke: %#v", invoke)
	}
	if strings.Contains(invoke.CallID, ":") {
		t.Fatalf("invalid call ID: %q", invoke.CallID)
	}
	wire, err := json.Marshal(invoke)
	if err != nil {
		t.Fatal(err)
	}
	var envelope map[string]json.RawMessage
	if err := json.Unmarshal(wire, &envelope); err != nil {
		t.Fatal(err)
	}
	if string(envelope["request_id"]) != `"42"` || string(envelope["call_id"]) != `"online_1"` || string(envelope["params"]) != `{"offline":false}` {
		t.Fatalf("wire: %s", wire)
	}
	terminal := m.handleTelegramResult(Message{Type: "telegram.result", CallID: invoke.CallID, OK: boolPtr(true)})
	if len(terminal) != 1 || terminal[0].Type != "event_result" || terminal[0].RequestID != "42" || m.pending != nil {
		t.Fatalf("terminal: %#v pending=%#v", terminal, m.pending)
	}
}

func TestDisabledAndMalformedTimerTicksDoNotInvoke(t *testing.T) {
	m := testModule(t)
	m.state.Enabled = false
	if responses := m.handleEvent(timerRequest("1", "tick")); len(responses) != 1 || responses[0].Type != "event_result" {
		t.Fatalf("disabled responses: %#v", responses)
	}
	m.state.Enabled = true
	if responses := m.handleEvent(Message{ProtocolVersion: protocolVersion, Type: "event", RequestID: "2", Event: "timer.tick", Payload: json.RawMessage(`{}`)}); len(responses) != 1 || responses[0].Type != "event_result" {
		t.Fatalf("malformed responses: %#v", responses)
	}
}

func TestOnlineCommandsAndIntervalGating(t *testing.T) {
	m := testModule(t)
	if _, err := m.execute("online", "interval 45"); err != nil || m.state.Interval != 45 {
		t.Fatalf("err=%v state=%#v", err, m.state)
	}
	if _, err := os.Stat(m.path); err != nil {
		t.Fatal(err)
	}
	if _, err := m.execute("online", "interval 29"); err == nil {
		t.Fatal("interval below core minimum must be rejected")
	}
	if _, err := m.execute("online", "off"); err != nil || m.state.Enabled {
		t.Fatalf("off err=%v enabled=%t", err, m.state.Enabled)
	}
	if text, err := m.execute("online", "status"); err != nil || !strings.Contains(text, "Configured interval: 45") {
		t.Fatalf("status text=%q err=%v", text, err)
	}
	if _, err := m.execute("online", "on"); err != nil {
		t.Fatal(err)
	}
	if len(m.handleEvent(timerRequest("1", "first"))) != 1 || m.pending == nil {
		t.Fatal("first tick must invoke")
	}
	m.handleTelegramResult(Message{Type: "telegram.result", CallID: "online_1", OK: boolPtr(true)})
	m.now = func() time.Time { return time.Unix(1030, 0) }
	if responses := m.handleEvent(timerRequest("2", "second")); len(responses) != 1 || responses[0].Type != "event_result" {
		t.Fatalf("interval must gate second tick: %#v", responses)
	}
	m.now = func() time.Time { return time.Unix(1045, 0) }
	if responses := m.handleEvent(timerRequest("3", "third")); len(responses) != 1 || responses[0].Type != "telegram.invoke" {
		t.Fatalf("interval must allow third tick: %#v", responses)
	}
}

func TestMalformedStateFallsBackToDefaults(t *testing.T) {
	directory := t.TempDir()
	t.Setenv("LAVIS_MODULE_STATE_DIR", directory)
	if err := os.WriteFile(filepath.Join(directory, "state.json"), []byte("not json"), 0o600); err != nil {
		t.Fatal(err)
	}
	m, err := loadModule()
	if err != nil {
		t.Fatal(err)
	}
	if m.state != defaultState() {
		t.Fatalf("state: %#v", m.state)
	}
}

func TestStatePersistsUnderModuleStateDirectory(t *testing.T) {
	directory := t.TempDir()
	t.Setenv("LAVIS_MODULE_STATE_DIR", directory)
	m, err := loadModule()
	if err != nil {
		t.Fatal(err)
	}
	if _, err := m.execute("online", "interval 90"); err != nil {
		t.Fatal(err)
	}
	if _, err := m.execute("online", "off"); err != nil {
		t.Fatal(err)
	}
	reloaded, err := loadModule()
	if err != nil {
		t.Fatal(err)
	}
	if reloaded.state.Enabled || reloaded.state.Interval != 90 || reloaded.path != filepath.Join(directory, "state.json") {
		t.Fatalf("reloaded state: %#v path=%q", reloaded.state, reloaded.path)
	}
}

func TestFloodWaitAndMalformedResultBackOffAndFinish(t *testing.T) {
	m := testModule(t)
	invoke := m.handleEvent(timerRequest("1", "tick"))[0]
	terminal := m.handleTelegramResult(Message{Type: "telegram.result", CallID: invoke.CallID, OK: boolPtr(false), Error: &TelegramError{Kind: "flood_wait", RetryAfterSeconds: 120, Message: "wait"}})
	if len(terminal) != 1 || m.state.BackoffUntil != 1120 || m.pending != nil {
		t.Fatalf("terminal=%#v state=%#v pending=%#v", terminal, m.state, m.pending)
	}
	m.state.BackoffUntil = 0
	m.state.LastInvokeAt = 0
	m.now = func() time.Time { return time.Unix(2000, 0) }
	m.handleEvent(timerRequest("2", "tick-2"))
	terminal = m.handleTelegramResult(Message{Type: "telegram.result", CallID: "online_2"})
	if len(terminal) != 1 || m.state.BackoffUntil != 2060 || m.pending != nil {
		t.Fatalf("malformed terminal=%#v state=%#v pending=%#v", terminal, m.state, m.pending)
	}
}

func TestTelegramResultRequiresStructuredError(t *testing.T) {
	if _, err := parseTelegramResult(Message{Type: "telegram.result", CallID: "call_1", OK: boolPtr(false)}); err == nil {
		t.Fatal("failed result without structured error must be rejected")
	}
	if _, err := parseTelegramResult(Message{Type: "telegram.result", CallID: "call_1", OK: boolPtr(true), Error: &TelegramError{Kind: "timeout", Message: "bad"}}); err == nil {
		t.Fatal("successful result with error must be rejected")
	}
}
