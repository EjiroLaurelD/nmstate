// SPDX-License-Identifier: Apache-2.0

use super::super::nm_dbus::{
    NmConnection, NmSettingConnection, NmSettingMacVlan, NmSettingVeth,
    NmSettingVlan, NmSettingVrf, NmSettingVxlan, NmSettingsConnectionFlag,
};
use super::{
    bond::gen_nm_bond_setting,
    bridge::{gen_nm_br_port_setting, gen_nm_br_setting},
    ethtool::gen_ethtool_setting,
    ieee8021x::gen_nm_802_1x_setting,
    infiniband::gen_nm_ib_setting,
    ip::gen_nm_ip_setting,
    mptcp::apply_mptcp_conf,
    ovs::{
        create_ovs_port_nm_conn, gen_nm_ovs_br_setting,
        gen_nm_ovs_ext_ids_setting, gen_nm_ovs_iface_setting,
    },
    sriov::gen_nm_sriov_setting,
    user::gen_nm_user_setting,
    veth::create_veth_peer_profile_if_not_found,
    wired::gen_nm_wired_setting,
};

use crate::{
    ErrorKind, Interface, InterfaceType, NetworkState, NmstateError,
    OvsBridgePortConfig,
};

pub(crate) const NM_SETTING_BRIDGE_SETTING_NAME: &str = "bridge";
pub(crate) const NM_SETTING_WIRED_SETTING_NAME: &str = "802-3-ethernet";
pub(crate) const NM_SETTING_OVS_BRIDGE_SETTING_NAME: &str = "ovs-bridge";
pub(crate) const NM_SETTING_OVS_PORT_SETTING_NAME: &str = "ovs-port";
pub(crate) const NM_SETTING_OVS_IFACE_SETTING_NAME: &str = "ovs-interface";
pub(crate) const NM_SETTING_VETH_SETTING_NAME: &str = "veth";
pub(crate) const NM_SETTING_BOND_SETTING_NAME: &str = "bond";
pub(crate) const NM_SETTING_DUMMY_SETTING_NAME: &str = "dummy";
pub(crate) const NM_SETTING_MACVLAN_SETTING_NAME: &str = "macvlan";
pub(crate) const NM_SETTING_VRF_SETTING_NAME: &str = "vrf";
pub(crate) const NM_SETTING_VLAN_SETTING_NAME: &str = "vlan";
pub(crate) const NM_SETTING_VXLAN_SETTING_NAME: &str = "vxlan";
pub(crate) const NM_SETTING_INFINIBAND_SETTING_NAME: &str = "infiniband";

pub(crate) const NM_SETTING_USER_SPACES: [&str; 2] = [
    NM_SETTING_OVS_BRIDGE_SETTING_NAME,
    NM_SETTING_OVS_PORT_SETTING_NAME,
];

pub(crate) fn iface_to_nm_connections(
    iface: &Interface,
    ctrl_iface: Option<&Interface>,
    exist_nm_conns: &[NmConnection],
    nm_ac_uuids: &[&str],
    veth_peer_exist_in_desire: bool,
    cur_net_state: &NetworkState,
) -> Result<Vec<NmConnection>, NmstateError> {
    let mut ret: Vec<NmConnection> = Vec::new();
    let base_iface = iface.base_iface();
    let exist_nm_conn = get_exist_profile(
        exist_nm_conns,
        &base_iface.name,
        &base_iface.iface_type,
        nm_ac_uuids,
    );
    if iface.is_up_exist_config() {
        if let Some(nm_conn) = exist_nm_conn {
            if !iface.is_userspace()
                && nm_conn.flags.contains(&NmSettingsConnectionFlag::External)
            {
                // User want to convert current state to persistent
                // But NetworkManager does not include routes for external
                // managed interfaces.
                if let Some(cur_iface) =
                    cur_net_state.get_kernel_iface_with_route(iface.name())
                {
                    // Do no try to persistent veth config of current interface
                    let mut iface = cur_iface;
                    if let Interface::Ethernet(eth_iface) = &mut iface {
                        eth_iface.veth = None;
                        eth_iface.base.iface_type = InterfaceType::Ethernet;
                    }

                    return iface_to_nm_connections(
                        &iface,
                        ctrl_iface,
                        exist_nm_conns,
                        nm_ac_uuids,
                        veth_peer_exist_in_desire,
                        cur_net_state,
                    );
                }
            }
            return Ok(vec![nm_conn.clone()]);
        } else if !iface.is_userspace() {
            if let Some(cur_iface) =
                cur_net_state.get_kernel_iface_with_route(iface.name())
            {
                // User want to convert unmanaged interface to managed
                if cur_iface.is_ignore() {
                    return iface_to_nm_connections(
                        &cur_iface,
                        ctrl_iface,
                        exist_nm_conns,
                        nm_ac_uuids,
                        veth_peer_exist_in_desire,
                        cur_net_state,
                    );
                }
            }
        }
    }
    let mut nm_conn = exist_nm_conn.cloned().unwrap_or_default();
    nm_conn.flags = Vec::new();

    // Use stable UUID if there is no existing NM connections where
    // we don't have possible UUID overlap there.
    // This enable us to generate the same output for `nm_gen_conf()`
    // when the desire state is the same.
    let stable_uuid = exist_nm_conns.is_empty();

    gen_nm_conn_setting(iface, &mut nm_conn, stable_uuid)?;
    gen_nm_ip_setting(
        iface,
        iface.base_iface().routes.as_deref(),
        iface.base_iface().rules.as_deref(),
        &mut nm_conn,
    )?;
    // InfiniBand over IP can not have layer 2 configuration.
    if iface.iface_type() != InterfaceType::InfiniBand {
        gen_nm_wired_setting(iface, &mut nm_conn);
    }
    gen_nm_ovs_ext_ids_setting(iface, &mut nm_conn);
    gen_nm_802_1x_setting(iface, &mut nm_conn);
    gen_nm_user_setting(iface, &mut nm_conn);
    gen_ethtool_setting(iface, &mut nm_conn)?;

    match iface {
        Interface::OvsBridge(ovs_br_iface) => {
            gen_nm_ovs_br_setting(ovs_br_iface, &mut nm_conn);
            // For OVS Bridge, we should create its OVS port also
            for ovs_port_conf in ovs_br_iface.port_confs() {
                let exist_nm_ovs_port_conn = get_exist_profile(
                    exist_nm_conns,
                    &ovs_port_conf.name,
                    &InterfaceType::Other("ovs-port".to_string()),
                    nm_ac_uuids,
                );
                ret.push(create_ovs_port_nm_conn(
                    &ovs_br_iface.base.name,
                    ovs_port_conf,
                    exist_nm_ovs_port_conn,
                    stable_uuid,
                )?)
            }
        }
        Interface::LinuxBridge(br_iface) => {
            gen_nm_br_setting(br_iface, &mut nm_conn);
        }
        Interface::Bond(bond_iface) => {
            gen_nm_bond_setting(bond_iface, &mut nm_conn);
        }
        Interface::OvsInterface(iface) => {
            gen_nm_ovs_iface_setting(iface, &mut nm_conn);
        }
        Interface::Vlan(vlan_iface) => {
            if let Some(conf) = vlan_iface.vlan.as_ref() {
                nm_conn.vlan = Some(NmSettingVlan::from(conf))
            }
        }
        Interface::Vxlan(vxlan_iface) => {
            if let Some(conf) = vxlan_iface.vxlan.as_ref() {
                nm_conn.vxlan = Some(NmSettingVxlan::from(conf))
            }
        }
        Interface::Ethernet(eth_iface) => {
            if let Some(veth_conf) = eth_iface.veth.as_ref() {
                nm_conn.veth = Some(NmSettingVeth::from(veth_conf));
                if !veth_peer_exist_in_desire {
                    // Create NM connect for veth peer so that
                    // veth could be in up state
                    ret.push(create_veth_peer_profile_if_not_found(
                        veth_conf.peer.as_str(),
                        eth_iface.base.name.as_str(),
                        exist_nm_conns,
                        stable_uuid,
                    )?);
                }
            }
            gen_nm_sriov_setting(eth_iface, &mut nm_conn);
        }
        Interface::MacVlan(iface) => {
            if let Some(conf) = iface.mac_vlan.as_ref() {
                nm_conn.mac_vlan = Some(NmSettingMacVlan::from(conf));
            }
        }
        Interface::MacVtap(iface) => {
            if let Some(conf) = iface.mac_vtap.as_ref() {
                nm_conn.mac_vlan = Some(NmSettingMacVlan::from(conf));
            }
        }
        Interface::Vrf(iface) => {
            if let Some(vrf_conf) = iface.vrf.as_ref() {
                nm_conn.vrf = Some(NmSettingVrf::from(vrf_conf));
            }
        }
        Interface::InfiniBand(iface) => {
            gen_nm_ib_setting(iface, &mut nm_conn);
        }
        _ => (),
    };

    if nm_conn.controller_type() != Some(NM_SETTING_BRIDGE_SETTING_NAME) {
        nm_conn.bridge_port = None;
    }

    if nm_conn.controller_type() != Some(NM_SETTING_OVS_PORT_SETTING_NAME) {
        nm_conn.ovs_iface = None;
    }

    if let Some(Interface::LinuxBridge(br_iface)) = ctrl_iface {
        gen_nm_br_port_setting(br_iface, &mut nm_conn);
    }

    // When detaching a OVS system interface from OVS bridge, we should remove
    // its NmSettingOvsIface setting
    if base_iface.controller.as_deref() == Some("") {
        nm_conn.ovs_iface = None;
    }

    // When user attaching new system port(ethernet) to existing OVS bridge
    // using `controller` property without OVS bridge mentioned in desire,
    // we need to create OVS port by ourselves.
    if iface.base_iface().controller_type.as_ref()
        == Some(&InterfaceType::OvsBridge)
        && ctrl_iface.is_none()
    {
        if let Some(ctrl_name) = iface.base_iface().controller.as_ref() {
            ret.push(create_ovs_port_nm_conn(
                ctrl_name,
                &OvsBridgePortConfig {
                    name: iface.name().to_string(),
                    ..Default::default()
                },
                None,
                stable_uuid,
            )?)
        }
    }

    ret.insert(0, nm_conn);

    Ok(ret)
}

pub(crate) fn iface_type_to_nm(
    iface_type: &InterfaceType,
) -> Result<String, NmstateError> {
    match iface_type {
        InterfaceType::LinuxBridge => Ok(NM_SETTING_BRIDGE_SETTING_NAME.into()),
        InterfaceType::Bond => Ok(NM_SETTING_BOND_SETTING_NAME.into()),
        InterfaceType::Ethernet => Ok(NM_SETTING_WIRED_SETTING_NAME.into()),
        InterfaceType::OvsBridge => {
            Ok(NM_SETTING_OVS_BRIDGE_SETTING_NAME.into())
        }
        InterfaceType::OvsInterface => {
            Ok(NM_SETTING_OVS_IFACE_SETTING_NAME.into())
        }
        InterfaceType::Vlan => Ok(NM_SETTING_VLAN_SETTING_NAME.to_string()),
        InterfaceType::Vxlan => Ok(NM_SETTING_VXLAN_SETTING_NAME.to_string()),
        InterfaceType::Dummy => Ok(NM_SETTING_DUMMY_SETTING_NAME.to_string()),
        InterfaceType::MacVlan => {
            Ok(NM_SETTING_MACVLAN_SETTING_NAME.to_string())
        }
        InterfaceType::MacVtap => {
            Ok(NM_SETTING_MACVLAN_SETTING_NAME.to_string())
        }
        InterfaceType::Vrf => Ok(NM_SETTING_VRF_SETTING_NAME.to_string()),
        InterfaceType::Veth => Ok(NM_SETTING_VETH_SETTING_NAME.to_string()),
        InterfaceType::InfiniBand => {
            Ok(NM_SETTING_INFINIBAND_SETTING_NAME.to_string())
        }
        InterfaceType::Other(s) => Ok(s.to_string()),
        _ => Err(NmstateError::new(
            ErrorKind::NotImplementedError,
            format!("Does not support iface type: {iface_type:?} yet"),
        )),
    }
}

pub(crate) fn gen_nm_conn_setting(
    iface: &Interface,
    nm_conn: &mut NmConnection,
    stable_uuid: bool,
) -> Result<(), NmstateError> {
    let mut nm_conn_set = if let Some(cur_nm_conn_set) = &nm_conn.connection {
        cur_nm_conn_set.clone()
    } else {
        let mut new_nm_conn_set = NmSettingConnection::default();
        let conn_name = match iface.iface_type() {
            InterfaceType::OvsBridge => {
                format!("{}-br", iface.name())
            }
            InterfaceType::Other(ref other_type)
                if other_type == "ovs-port" =>
            {
                format!("{}-port", iface.name())
            }
            InterfaceType::OvsInterface => {
                format!("{}-if", iface.name())
            }
            _ => iface.name().to_string(),
        };

        new_nm_conn_set.id = Some(conn_name);
        new_nm_conn_set.uuid = Some(if stable_uuid {
            uuid_from_name_and_type(iface.name(), &iface.iface_type())
        } else {
            // Use Linux random number generator (RNG) to generate UUID
            uuid::Uuid::new_v4().hyphenated().to_string()
        });
        new_nm_conn_set.iface_type =
            Some(iface_type_to_nm(&iface.iface_type())?);
        if let Interface::Ethernet(eth_iface) = iface {
            if eth_iface.veth.is_some() {
                new_nm_conn_set.iface_type =
                    Some(NM_SETTING_VETH_SETTING_NAME.to_string());
            }
        }
        new_nm_conn_set
    };

    nm_conn_set.iface_name = Some(iface.name().to_string());
    nm_conn_set.autoconnect = Some(true);
    nm_conn_set.autoconnect_ports = if iface.is_controller() {
        Some(true)
    } else {
        None
    };

    let nm_ctrl_type = iface
        .base_iface()
        .controller_type
        .as_ref()
        .map(iface_type_to_nm)
        .transpose()?;
    let nm_ctrl_type = nm_ctrl_type.as_deref();
    let ctrl_name = iface.base_iface().controller.as_deref();
    if let Some(ctrl_name) = ctrl_name {
        if ctrl_name.is_empty() {
            nm_conn_set.controller = None;
            nm_conn_set.controller_type = None;
        } else if let Some(nm_ctrl_type) = nm_ctrl_type {
            nm_conn_set.controller = Some(ctrl_name.to_string());
            nm_conn_set.controller_type = if nm_ctrl_type == "ovs-bridge"
                && iface.iface_type()
                    != InterfaceType::Other("ovs-port".to_string())
            {
                Some("ovs-port".to_string())
            } else {
                Some(nm_ctrl_type.to_string())
            };
        }
    }
    if let Some(lldp_conf) = iface.base_iface().lldp.as_ref() {
        nm_conn_set.lldp = Some(lldp_conf.enabled);
    }
    if let Some(mptcp_conf) = iface.base_iface().mptcp.as_ref() {
        apply_mptcp_conf(&mut nm_conn_set, mptcp_conf)?;
    }

    nm_conn.connection = Some(nm_conn_set);

    Ok(())
}

fn uuid_from_name_and_type(
    iface_name: &str,
    iface_type: &InterfaceType,
) -> String {
    uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("{iface_type}://{iface_name}").as_bytes(),
    )
    .hyphenated()
    .to_string()
}

// Found existing profile, prefer the activated one
pub(crate) fn get_exist_profile<'a>(
    exist_nm_conns: &'a [NmConnection],
    iface_name: &str,
    iface_type: &InterfaceType,
    nm_ac_uuids: &[&str],
) -> Option<&'a NmConnection> {
    let mut found_nm_conns: Vec<&NmConnection> = Vec::new();
    for exist_nm_conn in exist_nm_conns {
        let nm_iface_type = if let Ok(t) = iface_type_to_nm(iface_type) {
            t
        } else {
            continue;
        };
        if exist_nm_conn.iface_name() == Some(iface_name)
            && (exist_nm_conn.iface_type() == Some(&nm_iface_type)
                || (nm_iface_type == NM_SETTING_WIRED_SETTING_NAME
                    && exist_nm_conn.iface_type()
                        == Some(NM_SETTING_VETH_SETTING_NAME)))
        {
            if let Some(uuid) = exist_nm_conn.uuid() {
                // Prefer activated connection
                if nm_ac_uuids.contains(&uuid) {
                    return Some(exist_nm_conn);
                }
            }
            found_nm_conns.push(exist_nm_conn);
        }
    }
    found_nm_conns.pop()
}
