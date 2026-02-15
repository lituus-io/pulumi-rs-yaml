resource networkStack "pulumi:pulumi:StackReference" {
	__logicalName = "networkStack"
	name = "org/network/prod"
}

output vpcId {
	__logicalName = "vpcId"
	value = networkStack.outputs["vpcId"]
}
