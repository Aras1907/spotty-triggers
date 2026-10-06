package main

import (
	"context"
	"errors"
	"sync"
	"testing"
	"time"
)

func awaitBridgeExit(t *testing.T) {
	t.Helper()
	bridgeRuntime.Lock()
	done := bridgeRuntime.done
	bridgeRuntime.Unlock()
	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("Bridge runner did not return")
	}
}

func TestStartupErrorIsReturnedToHost(t *testing.T) {
	oldRunner := runBridge
	runBridge = func(context.Context, []string) error {
		return errors.New("synthetic initialization failure")
	}
	t.Cleanup(func() { runBridge = oldRunner })

	if got := startBridge(""); got >= 0 {
		t.Fatalf("startBridge returned %d, want startup failure", got)
	}
	awaitBridgeExit(t)
	if got := SpottyBridgeStatus(); got != failed {
		t.Fatalf("status = %d, want %d", got, failed)
	}
	if got := bridgeLastError(); got != "synthetic initialization failure" {
		t.Fatalf("last error = %q", got)
	}
}

func TestStopCancelsAndWaitsForRunner(t *testing.T) {
	oldRunner := runBridge
	entered := make(chan struct{})
	runBridge = func(ctx context.Context, _ []string) error {
		notifyOwnerAcquired()
		close(entered)
		<-ctx.Done()
		return nil
	}
	t.Cleanup(func() { runBridge = oldRunner })

	if got := startBridge(""); got != 0 {
		t.Fatalf("startBridge returned %d", got)
	}
	select {
	case <-entered:
	case <-time.After(2 * time.Second):
		t.Fatal("Bridge runner did not start")
	}
	started := time.Now()
	if got := stopBridge(); got != 0 {
		t.Fatalf("stopBridge returned %d", got)
	}
	if elapsed := time.Since(started); elapsed > time.Second {
		t.Fatalf("stopBridge took %s", elapsed)
	}
	if got := SpottyBridgeStatus(); got != stopped {
		t.Fatalf("status = %d, want %d", got, stopped)
	}
}

func TestStopHasBoundedWait(t *testing.T) {
	oldRunner := runBridge
	oldTimeout := stopTimeout
	release := make(chan struct{})
	var releaseOnce sync.Once
	runBridge = func(context.Context, []string) error {
		notifyOwnerAcquired()
		<-release
		return nil
	}
	stopTimeout = 40 * time.Millisecond
	t.Cleanup(func() {
		releaseOnce.Do(func() { close(release) })
		stopTimeout = oldTimeout
		runBridge = oldRunner
	})

	if got := startBridge(""); got != 0 {
		t.Fatalf("startBridge returned %d", got)
	}
	started := time.Now()
	if got := stopBridge(); got != -2 {
		t.Fatalf("stopBridge returned %d, want bounded timeout", got)
	}
	if elapsed := time.Since(started); elapsed > time.Second {
		t.Fatalf("bounded stop took %s", elapsed)
	}
	releaseOnce.Do(func() { close(release) })
	awaitBridgeExit(t)
}

func bridgeLastError() string {
	bridgeRuntime.Lock()
	defer bridgeRuntime.Unlock()
	return bridgeRuntime.lastErr
}
