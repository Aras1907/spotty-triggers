package app

var spottyOwnerAcquired func()

// SetSpottyOwnerAcquiredCallback is used by Spotty's C ABI adapter to confirm
// this instance acquired Bridge's profile lock before Start reports success.
func SetSpottyOwnerAcquiredCallback(callback func()) {
	spottyOwnerAcquired = callback
}

func notifySpottyOwnerAcquired() {
	if spottyOwnerAcquired != nil {
		spottyOwnerAcquired()
	}
}
