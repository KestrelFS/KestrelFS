# SPDX-License-Identifier: Apache-2.0

CC ?= cc
CFLAGS ?= -O2 -Wall -Wextra -Werror -std=gnu11

all: kestrelfs-cache-admin

kestrelfs-cache-admin: kestrelfs-cache-admin.c
	$(CC) $(CFLAGS) $< -o $@

clean:
	rm -f kestrelfs-cache-admin

.PHONY: all clean
