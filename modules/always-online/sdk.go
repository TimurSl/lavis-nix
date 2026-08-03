package main

import (
	"encoding/json"
	"fmt"
)

const protocolVersion = 5

// Message is the v5 JSONL envelope. Payload and params intentionally remain
// raw JSON: the wire protocol is the source of truth for their schemas.
type Message struct {
	ProtocolVersion int             `json:"protocol_version"`
	Type            string          `json:"type"`
	RequestID       string          `json:"request_id,omitempty"`
	ModuleID        string          `json:"module_id,omitempty"`
	Command         string          `json:"command,omitempty"`
	Arguments       string          `json:"arguments,omitempty"`
	Text            string          `json:"text,omitempty"`
	Event           string          `json:"event,omitempty"`
	Payload         json.RawMessage `json:"payload,omitempty"`
	CallID          string          `json:"call_id,omitempty"`
	Method          string          `json:"method,omitempty"`
	Params          json.RawMessage `json:"params,omitempty"`
	OK              *bool           `json:"ok,omitempty"`
	Result          json.RawMessage `json:"result,omitempty"`
	Error           *TelegramError  `json:"error,omitempty"`
	Code            string          `json:"code,omitempty"`
	Message         string          `json:"message,omitempty"`
}

// TelegramError keeps structured core failures intact, including timeout and
// FloodWait retry metadata when the core supplies it.
type TelegramError struct {
	Kind              string `json:"kind"`
	Code              *int64 `json:"code,omitempty"`
	Name              string `json:"name,omitempty"`
	Message           string `json:"message,omitempty"`
	RetryAfterSeconds int64  `json:"retry_after_seconds,omitempty"`
}

type TelegramResult struct {
	RequestID string
	CallID    string
	OK        bool
	Result    json.RawMessage
	Error     *TelegramError
}

func parseTelegramResult(message Message) (TelegramResult, error) {
	if message.Type != "telegram.result" {
		return TelegramResult{}, fmt.Errorf("not a telegram.result message")
	}
	if message.RequestID == "" || message.CallID == "" {
		return TelegramResult{}, fmt.Errorf("telegram.result missing request_id or call_id")
	}
	if message.OK == nil {
		return TelegramResult{}, fmt.Errorf("telegram.result missing ok")
	}
	if *message.OK && message.Error != nil {
		return TelegramResult{}, fmt.Errorf("successful telegram.result includes error")
	}
	if !*message.OK && (message.Error == nil || message.Error.Kind == "" || message.Error.Message == "") {
		return TelegramResult{}, fmt.Errorf("failed telegram.result missing error")
	}
	return TelegramResult{
		RequestID: message.RequestID,
		CallID:    message.CallID,
		OK:        *message.OK,
		Result:    message.Result,
		Error:     message.Error,
	}, nil
}

func telegramInvoke(requestID, callID string) Message {
	return Message{
		ProtocolVersion: protocolVersion,
		Type:            "telegram.invoke",
		RequestID:       requestID,
		CallID:          callID,
		Method:          "account.updateStatus",
		Params:          json.RawMessage(`{"offline":false}`),
	}
}

func reply(request Message, messageType string) Message {
	return Message{
		ProtocolVersion: protocolVersion,
		Type:            messageType,
		RequestID:       request.RequestID,
		ModuleID:        request.ModuleID,
	}
}
