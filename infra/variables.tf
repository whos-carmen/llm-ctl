variable "pve_endpoint" {
  type = string
}
variable "pve_user" {
  type = string
}
variable "pve_api_token" {
  type      = string
  sensitive = true
}
variable "node_name" {
  type    = string
  default = "px360"
}
variable "tag" {
  type    = string
  default = "ai"
}
variable "datastore_id" {
  type    = string
  default = "nvme1"
}
variable "template_file_id" {
  type    = string
  default = "local:vztmpl/debian-13-standard_13.6-1_amd64.tar.zst"
}
variable "bridge" {
  type    = string
  default = "vmbr0"
}
variable "netmask" {
  type    = number
  default = 22
}
variable "gateway" {
  type    = string
  default = "192.168.4.1"
}
variable "ssh_public_key_file" {
  type    = string
  default = "~/.ssh/id_ed25519.pub"
}

variable "containers" {
  description = "Ops-tier containers for this project (all tagged `ai`)."
  type = map(object({
    vmid     = number
    hostname = string
    ip       = string
    cores    = number   # vCPU
    memory   = number   # MiB
    disk     = number   # GiB
  }))
}