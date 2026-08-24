plugins {
    id("dev.kikugie.stonecutter")
    id("fabric-loom") version "1.7.4"
    `maven-publish`
}

val minecraftVersion = property("minecraft_version").toString()
val loaderVersion = property("loader_version").toString()
val fabricApiVersion = property("fabric_api_version").toString()
val javaVersion = property("java_version").toString().toInt()

group = property("maven_group").toString()
version = "${property("mod_version")}+${stonecutter.current.project}"

base {
    archivesName = property("archives_base_name").toString()
}

repositories {
    mavenCentral()
}

dependencies {
    minecraft("com.mojang:minecraft:$minecraftVersion")
    mappings(loom.officialMojangMappings())
    modImplementation("net.fabricmc:fabric-loader:$loaderVersion")
    modImplementation("net.fabricmc.fabric-api:fabric-api:$fabricApiVersion")
}

java {
    sourceCompatibility = JavaVersion.toVersion(javaVersion)
    targetCompatibility = JavaVersion.toVersion(javaVersion)
    withSourcesJar()
}

tasks.withType<JavaCompile>().configureEach {
    options.release = javaVersion
    options.encoding = "UTF-8"
}

tasks.processResources {
    inputs.properties(
        mapOf(
            "version" to project.version,
            "minecraft_version" to minecraftVersion,
            "loader_version" to loaderVersion,
        ),
    )
    filesMatching("fabric.mod.json") {
        expand(
            "version" to project.version,
            "minecraft_version" to minecraftVersion,
            "loader_version" to loaderVersion,
        )
    }
}

publishing {
    publications {
        create<MavenPublication>("mavenJava") {
            from(components["java"])
        }
    }
}
