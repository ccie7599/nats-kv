package adapter

import (
	"crypto/sha256"
	"hash"
)

type hashWriter = hash.Hash

func newSHA256() hashWriter { return sha256.New() }
