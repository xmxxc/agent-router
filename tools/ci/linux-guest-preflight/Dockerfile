ARG BASE_IMAGE
FROM ${BASE_IMAGE} AS guest
ARG KERNEL_RELEASE
COPY linux-image.deb linux-modules.deb /tmp/
RUN apt-get update \
    && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
         systemd-sysv initramfs-tools busybox-initramfs kmod e2fsprogs netcat-openbsd \
         libgcc-s1 libssl3 zlib1g jq /tmp/linux-modules.deb /tmp/linux-image.deb \
    && printf 'virtio_pci\nvirtio_blk\n' >> /etc/initramfs-tools/modules \
    && update-initramfs -u -k "$KERNEL_RELEASE" \
    && rm -r /var/lib/apt/lists \
    && rm /tmp/linux-image.deb /tmp/linux-modules.deb
COPY guest-preflight.sh /usr/local/libexec/wfc-guest-preflight.sh
COPY wfc-guest-preflight.service /etc/systemd/system/wfc-guest-preflight.service
COPY guest-config/ /
RUN useradd --uid 1000 --no-create-home --shell /usr/sbin/nologin wfc-guest \
    && mkdir -p /etc/systemd/system/multi-user.target.wants \
    && ln -s /etc/systemd/system/wfc-guest-preflight.service /etc/systemd/system/multi-user.target.wants/ \
    && ln -s /dev/null /etc/systemd/system/serial-getty@ttyS0.service

FROM guest AS disk
ARG KERNEL_RELEASE
COPY --from=guest / /guest
RUN mkdir /output \
    && cp "/guest/boot/vmlinuz-$KERNEL_RELEASE" /output/vmlinuz \
    && cp "/guest/boot/initrd.img-$KERNEL_RELEASE" /output/initrd.img \
    && truncate -s 768M /output/rootfs.ext4 \
    && mke2fs -q -F -t ext4 -d /guest /output/rootfs.ext4

FROM scratch
COPY --from=disk /output/ /
