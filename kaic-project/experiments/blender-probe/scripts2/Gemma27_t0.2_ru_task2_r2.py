import bpy

# Clear existing objects in the scene
bpy.ops.object.select_all(action='SELECT')
bpy.ops.object.delete()

# Create a cube at the origin
bpy.ops.mesh.primitive_cube_add(size=1, location=(0, 0, 0))
cube = bpy.context.active_object

# Create a new red material
mat = bpy.data.materials.new(name="RedMaterial")
mat.use_nodes = True
nodes = mat.node_tree.nodes
bsdf = nodes.get("Principled BSDF")
if bsdf:
    # Set the base color to red (RGBA)
    bsdf.inputs[0].default_value = (1, 0, 0, 1)

# Assign material to the cube
cube.data.materials.append(mat)

# Add a light source so the color is visible
bpy.ops.object.light_add(type='SUN', location=(5, 5, 5))

# Add a camera and position it to view the cube
bpy.ops.object.camera_add(location=(4, -4, 4), rotation=(1.1, 0, 0.785))
cam = bpy.context.active_object
bpy.context.scene.camera = cam

# Configure render settings
scene = bpy.context.scene
scene.render.resolution_x = 320
scene.render.resolution_y = 240
scene.render.image_settings.file_format = 'PNG'
scene.render.filepath = 'C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/Gemma27_t0.2_ru_r2/task2.png'

# Perform the render and save the image
bpy.ops.render.render(write_still=True)