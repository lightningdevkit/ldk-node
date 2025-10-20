import threading

from abc import ABC, abstractmethod
from typing import List

from ldk_node import IoError

class AbstractKvStore(ABC):
    @abstractmethod
    async def read_async(self, primary_namespace: "str",secondary_namespace: "str",key: "str") -> "typing.List[int]":
        pass
    
    @abstractmethod
    async def write_async(self, primary_namespace: "str",secondary_namespace: "str",key: "str",buf: "typing.List[int]") -> None:
        pass
    
    @abstractmethod
    async def remove_async(self, primary_namespace: "str",secondary_namespace: "str",key: "str",lazy: "bool") -> None:
        pass
    
    @abstractmethod
    async def list_async(self, primary_namespace: "str",secondary_namespace: "str") -> "typing.List[str]":
        pass

    @abstractmethod
    def read(self, primary_namespace: "str",secondary_namespace: "str",key: "str") -> "typing.List[int]":
        pass
    
    @abstractmethod
    def write(self, primary_namespace: "str",secondary_namespace: "str",key: "str",buf: "typing.List[int]") -> None:
        pass

    @abstractmethod
    def remove(self, primary_namespace: "str",secondary_namespace: "str",key: "str",lazy: "bool") -> None:
        pass
    
    @abstractmethod
    def list(self, primary_namespace: "str",secondary_namespace: "str") -> "typing.List[str]":
        pass

class TestKvStore(AbstractKvStore):
    def __init__(self, name: str):
        self.name = name
        # Storage structure: {(primary_ns, secondary_ns): {key: [bytes]}}
        self.storage = {}
        self._lock = threading.Lock()

    def dump(self):
        print(f"\n[{self.name}] Store contents:")
        for (primary_ns, secondary_ns), keys_dict in self.storage.items():
            print(f"  Namespace: ({primary_ns!r}, {secondary_ns!r})")
            for key, data in keys_dict.items():
                print(f"    Key: {key!r} -> {len(data)} bytes")
                # Optionally show first few bytes
                preview = data[:20] if len(data) > 20 else data
                print(f"      Data preview: {preview}...")

    def read(self, primary_namespace: str, secondary_namespace: str, key: str) -> List[int]:
        with self._lock:
            print(f"[{self.name}] READ: {primary_namespace}/{secondary_namespace}/{key}")
            namespace_key = (primary_namespace, secondary_namespace)
            
            if namespace_key not in self.storage:
                print(f"  -> namespace not found, keys: {list(self.storage.keys())}")
                raise IoError.NotFound(f"Namespace not found: {primary_namespace}/{secondary_namespace}")
            
            if key not in self.storage[namespace_key]:
                print(f"  -> key not found, keys: {list(self.storage[namespace_key].keys())}")
                raise IoError.NotFound(f"Key not found: {key}")

            data = self.storage[namespace_key][key]
            print(f"  -> returning {len(data)} bytes")
            return data

    def write(self, primary_namespace: str, secondary_namespace: str, key: str, buf: List[int]) -> None:
        with self._lock:
            namespace_key = (primary_namespace, secondary_namespace)
            if namespace_key not in self.storage:
                self.storage[namespace_key] = {}

            self.storage[namespace_key][key] = buf.copy()

    def remove(self, primary_namespace: str, secondary_namespace: str, key: str, lazy: bool) -> None:
        with self._lock:
            namespace_key = (primary_namespace, secondary_namespace)
            if namespace_key not in self.storage:
                raise IoError.NotFound(f"Namespace not found: {primary_namespace}/{secondary_namespace}")
        
            if key not in self.storage[namespace_key]:
                raise IoError.NotFound(f"Key not found: {key}")
        
            del self.storage[namespace_key][key]

            if not self.storage[namespace_key]:
                del self.storage[namespace_key]
                
    def list(self, primary_namespace: str, secondary_namespace: str) -> List[str]:
        with self._lock:
            namespace_key = (primary_namespace, secondary_namespace)
            if namespace_key in self.storage:
                return list(self.storage[namespace_key].keys())
            return []

    async def read_async(self, primary_namespace: str, secondary_namespace: str, key: str) -> List[int]:
        return self.read(primary_namespace, secondary_namespace, key)

    async def write_async(self, primary_namespace: str, secondary_namespace: str, key: str, buf: List[int]) -> None:
        self.write(primary_namespace, secondary_namespace, key, buf)

    async def remove_async(self, primary_namespace: str, secondary_namespace: str, key: str, lazy: bool) -> None:
        self.remove(primary_namespace, secondary_namespace, key, lazy)

    async def list_async(self, primary_namespace: str, secondary_namespace: str) -> List[str]:
        return self.list(primary_namespace, secondary_namespace)
   