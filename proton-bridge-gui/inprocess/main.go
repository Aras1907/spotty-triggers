//go:debug rsa1024min=0
package main

/*
#include <stdlib.h>
*/
import "C"

import (
	"context"
	"fmt"
	"os"
	"strings"
	"sync"
	"time"
	"unsafe"

	"github.com/ProtonMail/proton-bridge/v3/internal/app"
	"github.com/ProtonMail/proton-bridge/v3/internal/constants"
)

const (
	stopped = 0
	running = 1
	stopping = 2
	failed  = -1
)

var bridgeRuntime struct {
	sync.Mutex
	status   int
	cancel   context.CancelFunc
	done     chan struct{}
	lastErr  string
}

var runBridge = func(ctx context.Context, args []string) error {
	return app.New().RunContext(ctx, args)
}

var ownerAcquiredCallback func()

func notifyOwnerAcquired() {
	if ownerAcquiredCallback != nil {
		ownerAcquiredCallback()
	}
}

func main() {}

//export SpottyBridgeStart
func SpottyBridgeStart(launcher *C.char) C.int {
	launcherPath := ""
	if launcher != nil {
		launcherPath = C.GoString(launcher)
	}

	return C.int(startBridge(launcherPath))
}

func startBridge(launcherPath string) int {
	bridgeRuntime.Lock()
	if bridgeRuntime.status == running || bridgeRuntime.status == stopping {
		bridgeRuntime.Unlock()
		return -1
	}
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})
	ownerAcquired := make(chan struct{})
	var ownerOnce sync.Once
	ownerAcquiredCallback = func() {
		ownerOnce.Do(func() { close(ownerAcquired) })
	}
	app.SetSpottyOwnerAcquiredCallback(func() {
		notifyOwnerAcquired()
	})
	bridgeRuntime.cancel = cancel
	bridgeRuntime.done = done
	bridgeRuntime.status = running
	bridgeRuntime.lastErr = ""
	bridgeRuntime.Unlock()

	go func() {
		var runErr error
		defer func() {
			wasCancelled := ctx.Err() != nil
			cancel()
			app.SetSpottyOwnerAcquiredCallback(nil)
			ownerAcquiredCallback = nil
			if recovered := recover(); recovered != nil {
				runErr = fmt.Errorf("Bridge panic: %v", recovered)
			}
			bridgeRuntime.Lock()
			bridgeRuntime.cancel = nil
			if runErr != nil && !wasCancelled {
				bridgeRuntime.status = failed
				bridgeRuntime.lastErr = runErr.Error()
			} else {
				bridgeRuntime.status = stopped
			}
			close(done)
			bridgeRuntime.Unlock()
		}()

		args := []string{"spotty-proton-bridge", "--grpc", "--no-window"}
		if launcherPath != "" {
			args = append(args, "--launcher", launcherPath)
		}
		runErr = runBridge(ctx, args)
	}()

	select {
	case <-ownerAcquired:
		bridgeRuntime.Lock()
		started := bridgeRuntime.status == running
		if !started && bridgeRuntime.status == stopped {
			bridgeRuntime.status = failed
			bridgeRuntime.lastErr = "Bridge exited before acquiring its profile lock"
		}
		bridgeRuntime.Unlock()
		if started {
			return 0
		}
		return -1
	case <-done:
		bridgeRuntime.Lock()
		if bridgeRuntime.status == stopped {
			bridgeRuntime.status = failed
			bridgeRuntime.lastErr = "Bridge exited before acquiring its profile lock; another Bridge instance may own this profile"
		}
		bridgeRuntime.Unlock()
		return -1
	case <-time.After(120 * time.Second):
		cancel()
		bridgeRuntime.Lock()
		bridgeRuntime.lastErr = "Bridge did not acquire its profile lock within 120 seconds"
		bridgeRuntime.Unlock()
		select {
		case <-done:
			return -2
		case <-time.After(30 * time.Second):
			bridgeRuntime.Lock()
			bridgeRuntime.status = stopping
			bridgeRuntime.Unlock()
			return -2
		}
	}
}

//export SpottyBridgeStop
func SpottyBridgeStop() C.int {
	return C.int(stopBridge())
}

var stopTimeout = 30 * time.Second

func stopBridge() int {
	bridgeRuntime.Lock()
	if bridgeRuntime.status == stopped || bridgeRuntime.status == failed {
		bridgeRuntime.Unlock()
		return 0
	}
	bridgeRuntime.status = stopping
	cancel := bridgeRuntime.cancel
	done := bridgeRuntime.done
	bridgeRuntime.Unlock()

	if cancel != nil {
		cancel()
	}
	select {
	case <-done:
		return 0
	case <-time.After(stopTimeout):
		bridgeRuntime.Lock()
		bridgeRuntime.lastErr = fmt.Sprintf("Bridge did not stop within %s", stopTimeout)
		bridgeRuntime.Unlock()
		return -2
	}
}

//export SpottyBridgeStatus
func SpottyBridgeStatus() C.int {
	bridgeRuntime.Lock()
	status := bridgeRuntime.status
	bridgeRuntime.Unlock()
	return C.int(status)
}

//export SpottyBridgePID
func SpottyBridgePID() C.int {
	return C.int(os.Getpid())
}

//export SpottyBridgeVersion
func SpottyBridgeVersion() *C.char {
	return C.CString(constants.Version)
}

//export SpottyBridgeLastError
func SpottyBridgeLastError() *C.char {
	bridgeRuntime.Lock()
	message := strings.TrimSpace(bridgeRuntime.lastErr)
	bridgeRuntime.Unlock()
	return C.CString(message)
}

//export SpottyBridgeFree
func SpottyBridgeFree(value *C.char) {
	C.free(unsafe.Pointer(value))
}
