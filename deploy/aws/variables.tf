variable "name" {
  description = "The team's name, which prefixes every resource's name, such as `pumpkin-<name>-sandbox`."
  type        = string

  validation {
    condition     = can(regex("^[a-z]([a-z0-9-]{0,14}[a-z0-9])?$", var.name))
    error_message = "The name is not 1 to 16 of a-z, 0-9 and `-`, starting with a letter and ending with no `-`: give one such as `alice`."
  }
}

variable "vpc_id" {
  description = "The VPC of the sandboxes and of the controller that runs them."
  type        = string
}

variable "subnet_id" {
  description = "A public subnet of that VPC, which gives each instance launched in it a public IPv4 address: sandboxes pull from ECR through its internet gateway."
  type        = string
}

variable "modal_workspace" {
  description = "The ID of the team's Modal workspace, `ac-…`, whose tokens alone may assume the Modal pull role."
  type        = string

  validation {
    condition     = can(regex("^ac-[A-Za-z0-9]+$", var.modal_workspace))
    error_message = "The Modal workspace ID is not one: give one such as `ac-zrUhjAwtDAP4iR3aWkCHl2`."
  }
}

# The classes of sandbox: the instance type of each, and the CPU flags and model it offers a role,
# as `/proc/cpuinfo` names them. The flags are the x86-64 levels the instance's CPU meets (v2:
# cx16 to ssse3, v3: abm to xsave, v4: the avx512 ones), which the tasks' needs name. A class's
# `usd_per_hour` is what its instance type costs an hour in the region, as AWS's pricing page
# gives it.
variable "classes" {
  description = "The classes of sandbox, by name: each one's instance type, CPU flags and, for roles that require it alone, CPU model; and, if given, what an instance costs an hour, in US dollars."
  type = map(object({
    instance_type = string
    cpu_features  = set(string)
    cpu_model     = optional(string)
    usd_per_hour  = optional(number)
  }))

  # Each name is a bare TOML key of the settings and a Pumpkin name.
  validation {
    condition     = alltrue([for name in keys(var.classes) : can(regex("^[a-z][a-z0-9_-]{0,63}$", name))])
    error_message = "A class's name is not 1 to 64 of a-z, 0-9, `_` and `-`, starting with a letter: give one such as `judge_60`."
  }

  default = {
    # Targets of up to 8 CPUs (AMD EPYC 7R13, Zen 3).
    judge = {
      instance_type = "c6a.2xlarge"
      cpu_features  = ["abm", "avx", "avx2", "bmi1", "bmi2", "cx16", "f16c", "fma", "lahf_lm", "movbe", "popcnt", "sse4_1", "sse4_2", "ssse3", "xsave"]
    }
    # Agents and targets of up to 8 CPUs that need more memory than `judge` leaves them (Zen 3).
    memory = {
      instance_type = "r6a.2xlarge"
      cpu_features  = ["abm", "avx", "avx2", "bmi1", "bmi2", "cx16", "f16c", "fma", "lahf_lm", "movbe", "popcnt", "sse4_1", "sse4_2", "ssse3", "xsave"]
    }
    # Targets of 60 CPUs, and roles that need AVX-512 but no CPU model (AMD EPYC 9R14, Zen 4).
    judge_60 = {
      instance_type = "c7a.16xlarge"
      cpu_features  = ["abm", "avx", "avx2", "avx512bw", "avx512cd", "avx512dq", "avx512f", "avx512vl", "bmi1", "bmi2", "cx16", "f16c", "fma", "lahf_lm", "movbe", "popcnt", "sse4_1", "sse4_2", "ssse3", "xsave"]
    }
    # Roles sealed to the Xeon Platinum 8375C (Ice Lake), which no other role is placed in.
    ice_lake = {
      instance_type = "m6i.xlarge"
      cpu_features  = ["abm", "avx", "avx2", "avx512bw", "avx512cd", "avx512dq", "avx512f", "avx512vl", "bmi1", "bmi2", "cx16", "f16c", "fma", "lahf_lm", "movbe", "popcnt", "sse4_1", "sse4_2", "ssse3", "xsave"]
      cpu_model     = "Intel(R) Xeon(R) Platinum 8375C CPU @ 2.90GHz"
    }
  }
}

variable "root_volume_gib" {
  description = "The root volume of each sandbox's instance, in GiB, which holds its images and its workspace or scratch: 100 fits the GPU tasks' reference agents' 45 GB images."
  type        = number
  default     = 100
}

variable "controller_instance_type" {
  description = "The controller's instance type, which builds the tasks' images and runs the drivers."
  type        = string
  default     = "m7i.4xlarge"
}

variable "controller_volume_gib" {
  description = "The controller's root volume, in GiB, which holds the build cache and the images of recent tasks, at 20 to 70 GB a task, which each job prunes as the volume fills, the trees and the records."
  type        = number
  default     = 1000
}

variable "controller_volume_iops" {
  description = "The controller's root volume's IOPS, a gp3's provisioned I/O operations a second, which the builds of several tasks at once, each unpacking and writing its layers, and the prunes share: gp3's baseline is 3000."
  type        = number
  default     = 6000
}

variable "controller_volume_throughput" {
  description = "The controller's root volume's throughput, in MiB/s, a gp3's provisioned throughput, which the builds' layers and the pushes of their images read and write: gp3's baseline is 125."
  type        = number
  default     = 500
}

variable "pumpkin_release" {
  description = "A Pumpkin release's `pumpkinversion` line: its tag, its commit and its archive's SHA-256. The controller runs that release's `pumpkin-admin install --settings` at its first boot. A later release's install reaches a running controller only when its admin runs it there."
  type        = string

  validation {
    condition     = can(regex("^[a-zA-Z0-9][a-zA-Z0-9._-]* [0-9a-f]{40} [0-9a-f]{64}$", var.pumpkin_release))
    error_message = "The release is not a `pumpkinversion` line, `<tag> <commit> <sha256>`: give the line of a release's `pumpkinversion` asset, as a task repository's `.pumpkinversion` holds it."
  }
}

variable "ssh_from" {
  description = "The IPv4 CIDRs that may reach the controller's `sshd`, on port 22, such as each member's machine's private address as a `/32`: `pumpkin` reaches the controller at its private address, from the controller's VPC or a network routed to it. None by default, so no `pumpkin` reaches the controller."
  type        = list(string)
  default     = []

  validation {
    condition     = alltrue([for cidr in var.ssh_from : can(cidrnetmask(cidr))])
    error_message = "An entry of `ssh_from` is not an IPv4 CIDR: give one such as `10.0.1.23/32`."
  }
}

variable "members" {
  description = "The team's members, by name, each with the `ssh-ed25519` lines of their machines' `~/.config/pumpkin/ssh/key.pub`. The controller admits only these keys, and names each key's jobs by its member."
  type        = map(list(string))

  validation {
    condition     = length(var.members) > 0
    error_message = "`members` lists no one, so no key reaches the controller: list each member's key, such as `{ alice = [\"ssh-ed25519 AAAA… alice-laptop\"] }`."
  }

  validation {
    condition     = alltrue([for keys in values(var.members) : length(keys) > 0])
    error_message = "A member lists no key, so they can't reach the controller: list the line of each of their machines' `~/.config/pumpkin/ssh/key.pub`, or leave them out."
  }

  # The members' list holds a line for each key, so a key with a newline would start a line of its
  # own, under whatever member it names.
  validation {
    condition     = alltrue([for key in flatten(values(var.members)) : !strcontains(key, "\n")])
    error_message = "A member's key holds a newline, which would list a line of its own: give each key as the one line of its machine's `~/.config/pumpkin/ssh/key.pub`."
  }
}

variable "draining" {
  description = "Whether the controller's settings say `draining = true`, for a cutover. Each job that starts is then refused, and those that run go on to their ends."
  type        = bool
  default     = false
}
