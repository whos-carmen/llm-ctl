output "containers" {
  description = "Ops-tier containers for this project (tag `ai`)"
  value = {
    for k, c in var.containers : k => {
      vmid     = c.vmid
      hostname = c.hostname
      ip       = c.ip
      tag      = var.tag
    }
  }
}
