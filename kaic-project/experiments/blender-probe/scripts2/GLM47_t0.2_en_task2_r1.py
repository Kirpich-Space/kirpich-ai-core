import bpy

# Remove default objects
bpy.ops.object.select_all(action='SELECT')
bpy.ops.object.delete()

# Create Cube
bpy.ops.mesh.primitive_cube_add(size=2)
cube = bpy.context.active_object

# Create Red Material
mat = bpy.data.materials.new(name="RedMat")
mat.use_nodes = True
bsdf = mat.node_tree.nodes["Principled BSDF"]
bsdf.inputs["Base Color"].default_value = (1.0, 0.0, 0.0, 1.0)
cube.data.materials.append(mat)

# Create Camera
bpy.ops.object.camera_add(location=(0, 0, 5))
scene = bpy.context.scene
scene.camera = bpy.context.active_object

# Create Light
bpy.ops.object.light_add(type='POINT', location=(0, 0, 5))
light = bpy.context.active_object
light.data.energy = 1000

# Render Settings
scene.render.resolution_x = 320
scene.render.resolution_y = 240
scene.render.filepath = r"C:\Users\KIRPIC~1\AppData\Local\Temp\blender-probe\out2\GLM47_t0.2_en_r1\task2.png"
scene.render.image_settings.file_format = 'PNG'

# Render
bpy.ops.render.render(write_still=True)
