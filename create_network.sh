sudo nmcli connection add type tun con-name sp-tap0 ifname sp-tap0 \
  mode tap owner 1000 tun.multi-queue yes tun.vnet-hdr yes \
  ipv4.method manual ipv4.addresses 10.99.1.1/24 ipv6.method disabled \
  connection.autoconnect yes
sudo nmcli connection up sp-tap0
sudo firewall-cmd --permanent --zone=trusted --add-interface=sp-tap0
sudo firewall-cmd --reload


