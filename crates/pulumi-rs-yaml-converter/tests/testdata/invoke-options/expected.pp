curRegion = invoke("test:mod:Fn", {}, {
	parent = provider,
	provider = provider,
	version = "1.0.0",
	pluginDownloadUrl = "http://example.com"
})

resource provider "pulumi:providers:aws" {
	__logicalName = "provider"
	region = "us-west-2"
}
