#!/bin/sh

# Copyright (C) 2018  Braiins Systems s.r.o.
#
# This file is part of Braiins Build System (BB).
#
# BB is free software: you can redistribute it and/or modify
# it under the terms of the GNU General Public License as published by
# the Free Software Foundation, either version 3 of the License, or
# (at your option) any later version.
#
# This program is distributed in the hope that it will be useful,
# but WITHOUT ANY WARRANTY; without even the implied warranty of
# MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
# GNU General Public License for more details.
#
# You should have received a copy of the GNU General Public License
# along with this program.  If not, see <https://www.gnu.org/licenses/>.

if [ "$#" -ne 3 ]; then
    echo "Illegal number of parameters"
    exit 1
fi

set -e

MINER_HWID="$1"
KEEP_NET_CONFIG="$2"
KEEP_HOSTNAME="$3"

UBOOT_ENV_CFG="uboot_env.config"

SPL_IMAGE="boot.bin"
UBOOT_IMAGE="u-boot.img"
UBOOT_ENV_DATA="uboot_env.bin"
BITSTREAM_DATA="system.bit.gz"
KERNEL_IMAGE="fit.itb"
STAGE2_FIRMWARE="stage2.tgz"

sed_variables() {
    local value
    local args
    local input="$1"
    shift

    for name in "$@"; do
        eval value=\$$name
        args="$args -e 's,\${$name},$value,g'"
    done
    eval sed -i $args "$input"
}

# include firmware specific code
. ./CONTROL

# prepare configuration file
sed_variables "$UBOOT_ENV_CFG" UBOOT_ENV_MTD UBOOT_ENV1_OFF UBOOT_ENV2_OFF

flash_eraseall /dev/mtd${UBOOT_MTD} 2>&1

echo "Writing U-Boot images with FPGA bitstream..."
nandwrite -ps ${SPL_OFF} /dev/mtd${UBOOT_MTD} "$SPL_IMAGE" 2>&1
nandwrite -ps ${UBOOT_OFF} /dev/mtd${UBOOT_MTD} "$UBOOT_IMAGE" 2>&1
nandwrite -ps ${BITSTREAM_OFF} /dev/mtd${UBOOT_MTD} "$BITSTREAM_DATA" 2>&1

[ ${UBOOT_MTD} != ${UBOOT_ENV_MTD} ] && flash_eraseall /dev/mtd${UBOOT_ENV_MTD} 2>&1

echo "Writing U-Boot environment..."
nandwrite -ps ${UBOOT_ENV1_OFF} /dev/mtd${UBOOT_ENV_MTD} "$UBOOT_ENV_DATA" 2>&1
nandwrite -ps ${UBOOT_ENV2_OFF} /dev/mtd${UBOOT_ENV_MTD} "$UBOOT_ENV_DATA" 2>&1

flash_eraseall /dev/mtd${SRC_STAGE2_MTD} 2>&1

echo "Writing kernel image..."
nandwrite -ps ${SRC_KERNEL_OFF} /dev/mtd${SRC_STAGE2_MTD} "$KERNEL_IMAGE" 2>&1

echo "Writing stage2 tarball..."
nandwrite -ps ${SRC_STAGE2_OFF} /dev/mtd${SRC_STAGE2_MTD} "$STAGE2_FIRMWARE" 2>&1

echo "U-Boot configuration..."

# bitstream metadata
fw_setenv -c "$UBOOT_ENV_CFG" bitstream_off ${BITSTREAM_OFF}
fw_setenv -c "$UBOOT_ENV_CFG" bitstream_size $(file_size "$BITSTREAM_DATA")

# set kernel metadata
fw_setenv -c "$UBOOT_ENV_CFG" kernel_off ${DST_KERNEL_OFF}
fw_setenv -c "$UBOOT_ENV_CFG" kernel_size $(file_size "$KERNEL_IMAGE")

# set firmware stage2 metadata
fw_setenv -c "$UBOOT_ENV_CFG" stage2_off ${DST_STAGE2_OFF}
fw_setenv -c "$UBOOT_ENV_CFG" stage2_size $(file_size "$STAGE2_FIRMWARE")
fw_setenv -c "$UBOOT_ENV_CFG" stage2_mtd ${DST_STAGE2_MTD}

fw_setenv -c "$UBOOT_ENV_CFG" ethaddr ${ETHADDR}

# set network konfiguration
if [ x"$KEEP_NET_CONFIG" == x"yes" ]; then
    fw_setenv -c "$UBOOT_ENV_CFG" net_ip ${NET_IP}
    fw_setenv -c "$UBOOT_ENV_CFG" net_mask ${NET_MASK}
    fw_setenv -c "$UBOOT_ENV_CFG" net_gateway ${NET_GATEWAY}
    fw_setenv -c "$UBOOT_ENV_CFG" net_dns_servers ${NET_DNS_SERVERS}
fi
if [ x"$KEEP_HOSTNAME" == x"yes" ]; then
    fw_setenv -c "$UBOOT_ENV_CFG" net_hostname ${NET_HOSTNAME}
fi

# set miner configuration
fw_setenv -c "$UBOOT_ENV_CFG" miner_hwid ${MINER_HWID}

# s9 specific configuration
fw_setenv -c "$UBOOT_ENV_CFG" miner_freq ${MINER_FREQ}
fw_setenv -c "$UBOOT_ENV_CFG" miner_voltage ${MINER_VOLTAGE}
fw_setenv -c "$UBOOT_ENV_CFG" miner_fixed_freq ${MINER_FIXED_FREQ}

echo
echo "Content of U-Boot configuration:"
fw_printenv -c "$UBOOT_ENV_CFG"

sync
