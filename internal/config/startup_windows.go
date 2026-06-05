//go:build windows

package config

import "golang.org/x/sys/windows/registry"

const (
	startupRegKey  = `SOFTWARE\Microsoft\Windows\CurrentVersion\Run`
	startupAppName = "ClawdPanel"
)

func SetStartOnLogin(enabled bool, exePath string) error {
	key, err := registry.OpenKey(registry.CURRENT_USER, startupRegKey,
		registry.SET_VALUE|registry.QUERY_VALUE)
	if err != nil {
		return err
	}
	defer key.Close()
	if enabled {
		return key.SetStringValue(startupAppName, exePath)
	}
	return key.DeleteValue(startupAppName)
}

func IsStartOnLogin() bool {
	key, err := registry.OpenKey(registry.CURRENT_USER, startupRegKey, registry.QUERY_VALUE)
	if err != nil {
		return false
	}
	defer key.Close()
	_, _, err = key.GetStringValue(startupAppName)
	return err == nil
}
