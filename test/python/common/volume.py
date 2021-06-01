from common.hdl import MayastorHandle
from urllib.parse import urlparse


class Volume(object):
    """
    pool://hostname/pool_name

    The nvmt should be in the form of:

    nvmt://hostname
    """

    def __init__(self, uuid, nexus_node, pools, size):
        self.uuid = str(uuid)
        self.pools = pools
        self.nexus = nexus_node
        self.size = size

    def __parse_uri(self, uri):
        """
        private function that parses the URI
        """
        u = urlparse(uri)
        return (u.scheme, u.hostname, u.path[1:])

    def __create_replicas(self):
        """
        private function that creates the replica and shares it. Note that
        nvmf is used implicitly here

        """
        replicas = []
        for pool in self.pools:
            scheme, host, pool = self.__parse_uri(pool)
            assert scheme == "pool"
            handle = MayastorHandle(host)
            replicas.append(handle.replica_create(pool, self.uuid, self.size))
        return replicas

    def create(self) -> str:
        s, h, p = self.__parse_uri(self.nexus)
        assert s == "nvmt"

        replicas = [r.uri for r in self.__create_replicas()]
        handle = MayastorHandle(h)
        handle.nexus_create(self.uuid, self.size, replicas)
        return handle.nexus_publish(self.uuid).device_uri
