package api

import "syscall"

func statfs(path string) (*syscall.Statfs_t, error) {
	var s syscall.Statfs_t
	if err := syscall.Statfs(path, &s); err != nil {
		return nil, err
	}
	return &s, nil
}
