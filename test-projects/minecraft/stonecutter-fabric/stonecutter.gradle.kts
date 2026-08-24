plugins {
    id("dev.kikugie.stonecutter")
}

stonecutter active "1.21-fabric" /* [SC] DO NOT EDIT */

stonecutter registerChiseled tasks.register("buildAllVersions", stonecutter.chiseled) {
    group = "build"
    ofTask("build")
}
