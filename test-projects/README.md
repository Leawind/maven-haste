# Compatibility projects

These projects manually verify Maven Haste compatibility with Java and Minecraft build tools.
The Gradle fixtures form one multi-project build; the Stonecutter fixture is intentionally a
separate Gradle build because it owns its own version subprojects. They are not part of `cargo
test` and resolve public dependencies when run.

Start Maven Haste, then enter `test-projects` and run the Gradle build, or enter the Stonecutter
fixture directory for its independent build. All Gradle projects share the Wrapper in
`test-projects`:

```text
# Maven: run from test-projects/java/maven-basic
mvn --settings ../../../config-examples/maven-settings.xml verify

# Windows: all regular Gradle fixtures
gradlew.bat --init-script ../config-examples/gradle.init.gradle build

# Windows: the independent Stonecutter fixture (run from minecraft/stonecutter-fabric)
../../gradlew.bat --init-script ../../../config-examples/gradle.init.gradle buildAllVersions

# Linux/macOS: all regular Gradle fixtures
./gradlew --init-script ../config-examples/gradle.init.gradle build

# Linux/macOS: the independent Stonecutter fixture (run from minecraft/stonecutter-fabric)
../../gradlew --init-script ../../../config-examples/gradle.init.gradle buildAllVersions
```

The Gradle Wrapper uses 8.8 and all Gradle fixtures target JDK 21.
Run each project with a cold cache, a warm cache, and dependency refresh enabled to observe cache
hits and mutable metadata. Minecraft builds also download game files and assets outside Maven.
