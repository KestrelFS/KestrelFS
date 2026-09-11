# SPDX-License-Identifier: GPL-2.0
#
# Makefile for the KestrelFS out-of-tree kernel module (Phase 1).
#
# This ONLY builds our module against the prebuilt headers/symvers of
# the currently running kernel. It never recurses into a full kernel
# build (`make -j` on the kernel tree is forbidden by project policy).
#
# Usage:
#   make            # build kestrelfs.ko for the running kernel
#   make clean      # remove build artifacts
#   sudo make load  # insmod the freshly built module
#   sudo make unload # rmmod the module
#

obj-m += kestrelfs.o
kestrelfs-objs := super.o inode.o file.o chardev.o

KDIR ?= /lib/modules/$(shell uname -r)/build
PWD  := $(shell pwd)

all: modules

modules:
	$(MAKE) -C $(KDIR) M=$(PWD) modules

clean:
	$(MAKE) -C $(KDIR) M=$(PWD) clean

load: modules
	sudo insmod kestrelfs.ko

unload:
	sudo rmmod kestrelfs

.PHONY: all modules clean load unload
