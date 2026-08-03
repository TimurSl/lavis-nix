package main

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"
)

const (
	defaultInterval = int64(30)
	minimumInterval = int64(30)
	maximumInterval = int64(86400)
	defaultBackoff  = int64(60)
)

type State struct {
	Enabled      bool   `json:"enabled"`
	Interval     int64  `json:"interval_seconds"`
	BackoffUntil int64  `json:"backoff_until,omitempty"`
	LastInvokeAt int64  `json:"last_invoke_at,omitempty"`
	NextCallID   uint64 `json:"next_call_id"`
}

type pendingCall struct {
	requestID string
	callID    string
}

type Module struct {
	path    string
	state   State
	pending *pendingCall
	now     func() time.Time
}

func main() {
	m, err := loadModule()
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}

	scanner := bufio.NewScanner(os.Stdin)
	scanner.Buffer(make([]byte, 4096), 64*1024)
	encoder := json.NewEncoder(os.Stdout)
	encoder.SetEscapeHTML(false)
	for scanner.Scan() {
		var request Message
		if err := json.Unmarshal(scanner.Bytes(), &request); err != nil {
			continue
		}
		responses, stop := m.handle(request)
		for _, response := range responses {
			if err := encoder.Encode(response); err != nil {
				fmt.Fprintln(os.Stderr, err)
				return
			}
		}
		if stop {
			return
		}
	}
	if err := scanner.Err(); err != nil {
		fmt.Fprintln(os.Stderr, err)
	}
}

func loadModule() (*Module, error) {
	directory := os.Getenv("LAVIS_MODULE_STATE_DIR")
	if directory == "" {
		return nil, errors.New("LAVIS_MODULE_STATE_DIR is required")
	}
	m := &Module{
		path:  filepath.Join(directory, "state.json"),
		state: defaultState(),
		now:   time.Now,
	}
	data, err := os.ReadFile(m.path)
	if errors.Is(err, os.ErrNotExist) {
		return m, nil
	}
	if err != nil {
		return nil, fmt.Errorf("read state: %w", err)
	}
	var restored State
	if err := json.Unmarshal(data, &restored); err != nil || !validState(restored) {
		// A malformed state file must not prevent the module from starting.
		return m, nil
	}
	m.state = restored
	return m, nil
}

func defaultState() State {
	return State{Enabled: true, Interval: defaultInterval, NextCallID: 1}
}

func validState(state State) bool {
	return state.Interval >= minimumInterval &&
		state.Interval <= maximumInterval &&
		state.BackoffUntil >= 0 &&
		state.LastInvokeAt >= 0 &&
		state.NextCallID > 0
}

func (m *Module) handle(request Message) ([]Message, bool) {
	if request.ProtocolVersion != protocolVersion {
		response := moduleError(request, "PROTOCOL_VERSION", "unsupported protocol version")
		return []Message{response}, false
	}
	switch request.Type {
	case "initialize":
		return []Message{reply(request, "initialized")}, false
	case "health":
		return []Message{reply(request, "health")}, false
	case "shutdown":
		return []Message{reply(request, "shutdown")}, true
	case "execute":
		response := reply(request, "result")
		text, err := m.execute(request.Command, request.Arguments)
		if err != nil {
			response = moduleError(request, "BAD_INPUT", err.Error())
		} else {
			response.Text = text
		}
		return []Message{response}, false
	case "event":
		return m.handleEvent(request), false
	case "telegram.result":
		return m.handleTelegramResult(request), false
	default:
		response := moduleError(request, "UNKNOWN_TYPE", "unknown request type")
		return []Message{response}, false
	}
}

func (m *Module) handleEvent(request Message) []Message {
	response := reply(request, "event_result")
	if request.Event != "timer.tick" || !m.state.Enabled || m.pending != nil {
		return []Message{response}
	}
	var payload struct {
		EventID string `json:"event_id"`
	}
	if err := json.Unmarshal(request.Payload, &payload); err != nil || strings.TrimSpace(payload.EventID) == "" {
		return []Message{response}
	}
	now := m.now().Unix()
	if now < m.state.BackoffUntil ||
		m.state.LastInvokeAt > now ||
		now-m.state.LastInvokeAt < m.state.Interval {
		return []Message{response}
	}
	callID := fmt.Sprintf("online_%d", m.state.NextCallID)
	m.state.NextCallID++
	m.state.LastInvokeAt = now
	if err := m.save(); err != nil {
		return []Message{moduleError(request, "STATE_WRITE", err.Error())}
	}
	m.pending = &pendingCall{requestID: request.RequestID, callID: callID}
	return []Message{telegramInvoke(request.RequestID, callID)}
}

func (m *Module) handleTelegramResult(message Message) []Message {
	result, err := parseTelegramResult(message)
	if err != nil {
		return m.finishMalformedResult()
	}
	if m.pending == nil ||
		result.RequestID != m.pending.requestID ||
		result.CallID != m.pending.callID {
		return m.finishMalformedResult()
	}
	requestID := m.pending.requestID
	m.pending = nil
	if !result.OK {
		m.applyBackoff(result.Error)
	}
	return []Message{{ProtocolVersion: protocolVersion, Type: "event_result", RequestID: requestID}}
}

func (m *Module) finishMalformedResult() []Message {
	if m.pending == nil {
		return nil
	}
	requestID := m.pending.requestID
	m.pending = nil
	m.applyBackoff(nil)
	return []Message{{ProtocolVersion: protocolVersion, Type: "event_result", RequestID: requestID}}
}

func (m *Module) applyBackoff(problem *TelegramError) {
	backoff := retryDelay(problem)
	m.state.BackoffUntil = m.now().Add(time.Duration(backoff) * time.Second).Unix()
	if err := m.save(); err != nil {
		fmt.Fprintln(os.Stderr, "save backoff state:", err)
	}
}

func retryDelay(problem *TelegramError) int64 {
	if problem == nil {
		return defaultBackoff
	}
	if problem.RetryAfterSeconds > 0 {
		return problem.RetryAfterSeconds
	}
	kind := strings.ToUpper(problem.Kind)
	name := strings.ToUpper(problem.Name)
	if strings.Contains(kind, "FLOOD") ||
		strings.Contains(kind, "TEMPORARY") ||
		strings.Contains(kind, "TIMEOUT") ||
		strings.Contains(name, "FLOOD") {
		return defaultBackoff
	}
	return defaultBackoff
}

func moduleError(request Message, code, message string) Message {
	response := reply(request, "error")
	response.Code = code
	response.Message = message
	return response
}

func (m *Module) execute(command, arguments string) (string, error) {
	if strings.ToLower(strings.TrimSpace(command)) != "online" {
		return "", fmt.Errorf("unknown command: %s", command)
	}
	fields := strings.Fields(arguments)
	if len(fields) == 0 || (len(fields) == 1 && strings.EqualFold(fields[0], "status")) {
		return m.status(), nil
	}
	if len(fields) == 1 {
		switch strings.ToLower(fields[0]) {
		case "on":
			m.state.Enabled = true
		case "off":
			m.state.Enabled = false
		default:
			return "", errors.New("usage: online [on|off|status|interval <seconds>]")
		}
		if err := m.save(); err != nil {
			return "", err
		}
		return m.status(), nil
	}
	if len(fields) == 2 && strings.EqualFold(fields[0], "interval") {
		interval, err := strconv.ParseInt(fields[1], 10, 64)
		if err != nil || interval < minimumInterval || interval > maximumInterval {
			return "", fmt.Errorf("interval must be between %d and %d seconds", minimumInterval, maximumInterval)
		}
		m.state.Interval = interval
		if err := m.save(); err != nil {
			return "", err
		}
		return m.status(), nil
	}
	return "", errors.New("usage: online [on|off|status|interval <seconds>]")
}

func (m *Module) status() string {
	state := "off"
	if m.state.Enabled {
		state = "on"
	}
	return fmt.Sprintf("Always Online: %s\nConfigured interval: %d seconds\nCore timer interval: %d seconds", state, m.state.Interval, defaultInterval)
}

func (m *Module) save() error {
	data, err := json.MarshalIndent(m.state, "", "  ")
	if err != nil {
		return fmt.Errorf("encode state: %w", err)
	}
	if err := os.MkdirAll(filepath.Dir(m.path), 0o700); err != nil {
		return fmt.Errorf("create state directory: %w", err)
	}
	temporary, err := os.CreateTemp(filepath.Dir(m.path), ".state.json.tmp-")
	if err != nil {
		return fmt.Errorf("create temporary state: %w", err)
	}
	temporaryPath := temporary.Name()
	defer os.Remove(temporaryPath)
	if err := temporary.Chmod(0o600); err != nil {
		temporary.Close()
		return fmt.Errorf("secure temporary state: %w", err)
	}
	if _, err := temporary.Write(append(data, '\n')); err != nil {
		temporary.Close()
		return fmt.Errorf("write state: %w", err)
	}
	if err := temporary.Sync(); err != nil {
		temporary.Close()
		return fmt.Errorf("sync state: %w", err)
	}
	if err := temporary.Close(); err != nil {
		return fmt.Errorf("close state: %w", err)
	}
	if err := os.Rename(temporaryPath, m.path); err != nil {
		return fmt.Errorf("replace state: %w", err)
	}
	directory, err := os.Open(filepath.Dir(m.path))
	if err != nil {
		return fmt.Errorf("open state directory: %w", err)
	}
	defer directory.Close()
	if err := directory.Sync(); err != nil {
		return fmt.Errorf("sync state directory: %w", err)
	}
	return nil
}
