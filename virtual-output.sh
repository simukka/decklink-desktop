#!/bin/sh
# Create a virtual RandR output (vkms) for decklink-desktop to play out.
#
# vkms is a kernel "virtual KMS" GPU with one virtual connector. The X server
# hotplugs it as an extra provider; linking it to the real GPU makes its
# output (Virtual-1-1 or similar) appear in xrandr/arandr like a monitor.
# Nothing is ever displayed by vkms itself: decklink-desktop --output reads
# that output's area of the desktop and sends it to the DeckLink card.
set -eu

if ! lsmod | grep -q '^vkms '; then
    echo "Loading vkms (needs sudo)..."
    sudo modprobe vkms
fi

# Wait for X to pick up the new DRM device.
i=0
until xrandr --listproviders | grep -q 'name:.*vkms'; do
    i=$((i + 1))
    if [ "$i" -gt 40 ]; then
        echo "X did not hotplug the vkms device; see ~/.local/share/xorg/Xorg.0.log" >&2
        exit 1
    fi
    sleep 0.25
done

provider_id() { xrandr --listproviders | awk -v pat="$1" '$0 ~ pat { print $4; exit }'; }
sink=$(provider_id 'name:.*vkms')
source=$(provider_id 'Source Output')
xrandr --setprovideroutputsource "$sink" "$source"

echo
xrandr --listproviders
echo
xrandr | grep -iE '^virtual' || true
