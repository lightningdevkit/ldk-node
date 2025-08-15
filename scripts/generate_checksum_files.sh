#!/bin/bash
md5sum $1 | cut -d ' ' -f 1 > $1.md5
sha1sum $1 | cut -d ' ' -f 1 > $1.sha1
sha256sum $1 | cut -d ' ' -f 1 > $1.sha256
sha512sum $1 | cut -d ' ' -f 1 > $1.sha512
