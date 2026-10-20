set -eu
JQ=/nix/store/m3y7hqmy20nd1ps6qy8410a326255bf0-jq-1.8.2-bin/bin/jq
REGION=$("$JQ" -er '.region' /tmp/shared/wayland-region.json)
HOST_RUN=$(cat /tmp/shared/host-run.txt)
mkdir -p /tmp/product-rdd-fixture
chown 1000:100 /tmp/product-rdd-fixture
set +e
runuser -u alice -- env WC_BROKER=/run/private-cuse/wayland-broker/broker.sock PRODUCT_WAYLAND_REGION="broker:$REGION" PRODUCT_WAYLAND_RUN="$HOST_RUN" PRODUCT_WAYLAND_BELL=/run/private-cuse/user/wayland-bell.sock SV_SOCKET=/run/private-cuse/user/gpu/gpu.sock SV_BELL=/run/private-cuse/user/bell.sock SV_DRM_NODE=/dev/dri/renderD190 timeout --signal=TERM --kill-after=5 165 bash /run/product-snapshot/assets/firefox-payload
echo FIREFOX_PAYLOAD_STATUS=$?
tar -cf /tmp/shared/firefox-evidence.tar -C /tmp product-rdd-fixture
